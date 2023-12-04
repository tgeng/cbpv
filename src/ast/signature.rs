use std::cmp::{min, Ordering};
use std::collections::{HashMap};
use crate::ast::free_var::HasFreeVar;
use crate::ast::primitive_functions::{PRIMITIVE_FUNCTIONS, PrimitiveFunction};
use crate::ast::term::{CTerm, CType, VTerm, VType};
use crate::ast::transformer::Transformer;

#[derive(Debug, Clone)]
pub struct FunctionDefinition {
    pub args: Vec<(usize, VType)>,
    pub body: CTerm,
    pub c_type: CType,
    /// The (exclusive) upperbound of local variables bound in this definition. This is useful to
    /// initialize an array, which can be guaranteed to fit all local variables in this function.
    pub var_bound: usize,
    /// This function may be simple (after specialization). In this case a simple version of the
    /// function is generated for faster calls.
    pub may_be_simple: bool,
    pub may_be_complex: bool,
}

impl FunctionDefinition {
    pub fn is_specializable(&self) -> bool {
        self.may_be_simple && matches!(self.c_type, CType::SpecializedF(_))
    }
}

pub struct Signature {
    pub defs: HashMap<String, FunctionDefinition>,
}

impl Signature {
    pub fn new() -> Self {
        Self {
            defs: HashMap::new(),
        }
    }

    pub fn into_defs(self) -> HashMap<String, FunctionDefinition> {
        self.defs
    }

    pub fn insert(&mut self, name: String, def: FunctionDefinition) {
        self.defs.insert(name, def);
    }

    pub fn optimize(&mut self) {
        self.reduce_redundancy();
        self.specialize_calls();
        self.rename_local_vars();
        self.reduce_immediate_redexes();
        self.lift_lambdas();
        self.rename_local_vars();
    }

    fn reduce_redundancy(&mut self) {
        let mut normalizer = RedundancyRemover {};
        self.defs.iter_mut().for_each(|(_, FunctionDefinition { body, .. })| {
            normalizer.transform_c_term(body);
        });
    }


    fn specialize_calls(&mut self) {
        let mut new_defs: Vec<(String, FunctionDefinition)> = Vec::new();
        let specializable_functions: HashMap<_, _> = self.defs.iter()
            .filter_map(|(name, FunctionDefinition { args, body, c_type, var_bound, may_be_simple, .. })| {
                if let CType::SpecializedF(_) = c_type && *may_be_simple {
                    Some((name.clone(), args.len()))
                } else {
                    None
                }
            }).collect();

        self.defs.iter_mut().for_each(|(name, FunctionDefinition { args, body, .. })| {
            let mut specializer = CallSpecializer { def_name: name, new_defs: &mut new_defs, primitive_wrapper_counter: 0, specializable_functions: &specializable_functions };
            specializer.transform_c_term(body);
        });
        self.insert_new_defs(new_defs);
    }

    /// Assume all local variables are distinct. Also this transformation preserves this property.
    fn reduce_immediate_redexes(&mut self) {
        let mut reducer = RedexReducer {};
        self.defs.iter_mut().for_each(|(_, FunctionDefinition { body, .. })| {
            reducer.transform_c_term(body);
        });
    }

    /// Assume all local variables are distinct. Also this transformation preserves this property.
    fn lift_lambdas(&mut self) {
        let mut new_defs: Vec<(String, FunctionDefinition)> = Vec::new();
        self.defs.iter_mut().for_each(|(name, FunctionDefinition { args, body, var_bound, .. })| {
            let local_var_types = &mut vec![VType::Uniform; *var_bound];
            for (i, ty) in args {
                local_var_types[*i] = *ty;
            }
            let lifter = LambdaLifter { def_name: name, counter: 0, new_defs: &mut new_defs, local_var_types };
            let mut thunk_lifter = lifter;
            thunk_lifter.transform_c_term(body);
        });
        self.insert_new_defs(new_defs);
    }

    fn insert_new_defs(&mut self, new_defs: Vec<(String, FunctionDefinition)>) {
        for (name, FunctionDefinition {
            mut args,
            mut body,
            c_type,
            mut var_bound,
            may_be_simple,
            may_be_complex
        }) in new_defs.into_iter() {
            Self::rename_local_vars_in_def(&mut args, &mut body, &mut var_bound);
            self.insert(name, FunctionDefinition {
                args,
                body,
                c_type,
                var_bound,
                may_be_simple,
                may_be_complex,
            })
        }
    }

    fn rename_local_vars(&mut self) {
        self.defs.iter_mut().for_each(|(_, FunctionDefinition { args, body, var_bound: max_arg_size, .. })| {
            Self::rename_local_vars_in_def(args, body, max_arg_size);
        });
    }
    fn rename_local_vars_in_def(args: &mut [(usize, VType)], body: &mut CTerm, var_bound: &mut usize) {
        let mut renamer = DistinctVarRenamer { bindings: HashMap::new(), counter: 0 };
        for (i, _) in args.iter_mut() {
            *i = renamer.add_binding(*i);
        }
        renamer.transform_c_term(body);
        *var_bound = renamer.counter;
    }
}

struct DistinctVarRenamer {
    bindings: HashMap<usize, Vec<usize>>,
    counter: usize,
}

impl Transformer for DistinctVarRenamer {
    fn add_binding(&mut self, index: usize) -> usize {
        let indexes = self.bindings.entry(index).or_default();
        let new_index = self.counter;
        indexes.push(new_index);
        self.counter += 1;
        new_index
    }

    fn remove_binding(&mut self, index: usize) {
        self.bindings.get_mut(&index).unwrap().pop();
    }

    fn transform_var(&mut self, v_term: &mut VTerm) {
        match v_term {
            VTerm::Var { index: name } => {
                if let Some(bindings) = self.bindings.get_mut(name) {
                    if !bindings.is_empty() {
                        *name = *bindings.last().unwrap();
                    }
                }
            }
            _ => unreachable!(),
        }
    }
}

struct RedundancyRemover {}

impl Transformer for RedundancyRemover {
    fn transform_redex(&mut self, c_term: &mut CTerm) {
        self.transform_redex_default(c_term);
        match c_term {
            CTerm::Redex { function, args } => {
                if args.is_empty() {
                    let mut placeholder = CTerm::Return { value: VTerm::Int { value: 1 } };
                    std::mem::swap(&mut placeholder, function);
                    *c_term = placeholder;
                } else {
                    let is_nested_redex = matches!(function.as_ref(), CTerm::Redex { .. });
                    if is_nested_redex {
                        let mut placeholder = CTerm::Return { value: VTerm::Int { value: 1 } };
                        std::mem::swap(&mut placeholder, c_term);
                        match placeholder {
                            CTerm::Redex { function, args } => {
                                match *function {
                                    CTerm::Redex { function: sub_function, args: sub_args } => {
                                        *c_term = CTerm::Redex { function: sub_function, args: sub_args.into_iter().chain(args).collect() };
                                    }
                                    _ => unreachable!(),
                                }
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    fn transform_lambda(&mut self, c_term: &mut CTerm) {
        self.transform_lambda_default(c_term);
        let CTerm::Lambda { args, box body, may_have_complex_effects } = c_term else { unreachable!() };
        if let CTerm::Lambda { args: sub_args, body: box sub_body, may_have_complex_effects: sub_may_have_complex_effects } = body {
            *may_have_complex_effects |= *sub_may_have_complex_effects;
            args.extend(sub_args.iter().copied());
            let mut placeholder = CTerm::Return { value: VTerm::Int { value: 0 } };
            std::mem::swap(&mut placeholder, sub_body);
            *body = placeholder
        }
        if args.is_empty() {
            let mut placeholder = CTerm::Return { value: VTerm::Int { value: 0 } };
            std::mem::swap(&mut placeholder, body);
            *c_term = placeholder;
        }
    }
}

struct CallSpecializer<'a> {
    def_name: &'a str,
    new_defs: &'a mut Vec<(String, FunctionDefinition)>,
    primitive_wrapper_counter: usize,
    specializable_functions: &'a HashMap<String, usize>,
}

impl<'a> Transformer for CallSpecializer<'a> {
    fn transform_redex(&mut self, c_term: &mut CTerm) {
        self.transform_redex_default(c_term);
        let CTerm::Redex { box function, args } = c_term else { unreachable!() };
        let CTerm::Def { name, may_have_complex_effects: has_handler_effects } = function else { return; };
        if let Some((name, PrimitiveFunction { arg_types, return_type, .. })) = PRIMITIVE_FUNCTIONS.get_entry(name) {
            match arg_types.len().cmp(&args.len()) {
                Ordering::Greater => {
                    let primitive_wrapper_name = format!("{}$__primitive_wrapper_{}", self.def_name, self.primitive_wrapper_counter);
                    // Primitive calls cannot be effectful.
                    assert!(!*has_handler_effects);
                    *function = CTerm::Def { name: primitive_wrapper_name.clone(), may_have_complex_effects: false };
                    self.new_defs.push((primitive_wrapper_name, FunctionDefinition {
                        args: arg_types.iter().enumerate().map(|(i, t)| (i, *t)).collect(),
                        body: CTerm::PrimitiveCall {
                            name,
                            args: (0..arg_types.len()).map(|index| VTerm::Var { index }).collect(),
                        },
                        c_type: CType::SpecializedF(*return_type),
                        var_bound: arg_types.len(),
                        may_be_simple: true,
                        may_be_complex: false,
                    }))
                }
                Ordering::Equal => {
                    let CTerm::Redex { args, .. } = std::mem::replace(
                        c_term,
                        CTerm::PrimitiveCall { name, args: vec![] },
                    ) else { unreachable!() };
                    let CTerm::PrimitiveCall { args: new_args, .. } = c_term else { unreachable!() };
                    *new_args = args;
                }
                Ordering::Less => {
                    unreachable!()
                }
            }
        }
    }
}

struct LambdaLifter<'a> {
    def_name: &'a str,
    counter: usize,
    new_defs: &'a mut Vec<(String, FunctionDefinition)>,
    local_var_types: &'a mut [VType],
}

impl<'a> LambdaLifter<'a> {
    fn create_new_redex(&mut self, free_vars: &[usize], may_have_complex_effects: bool) -> (String, CTerm) {
        let thunk_def_name = format!("{}$__lambda_{}", self.def_name, self.counter);
        self.counter += 1;

        let redex =
            CTerm::Redex {
                function: Box::new(CTerm::Def { name: thunk_def_name.clone(), may_have_complex_effects }),
                args: free_vars.iter().map(|i| VTerm::Var { index: *i }).collect(),
            };
        (thunk_def_name, redex)
    }

    fn create_new_def(&mut self, name: String, free_vars: Vec<usize>, args: &[(usize, VType)], body: CTerm, may_be_complex: bool) {
        let var_bound = *free_vars.iter().max().unwrap_or(&0);
        let function_definition = FunctionDefinition {
            args: free_vars.into_iter().map(|v| (v, self.local_var_types[v])).chain(args.iter().copied()).collect(),
            body,
            c_type: CType::Default,
            var_bound,
            // We need to generate simple flavor for redex calling this function.
            may_be_simple: !may_be_complex,
            // All thunks are treated as effectful to simplify compilation.
            may_be_complex: true,
        };
        self.new_defs.push((name, function_definition));
    }
}

impl<'a> Transformer for LambdaLifter<'a> {
    fn transform_thunk(&mut self, v_term: &mut VTerm) {
        let VTerm::Thunk { t: box c_term, may_have_complex_effects } = v_term else { unreachable!() };
        self.transform_c_term(c_term);

        if let CTerm::Redex { function: box CTerm::Def { .. }, .. } | CTerm::Def { .. } = c_term {
            // There is no need to lift the thunk if it's already a simple function call.
            return;
        }

        let mut free_vars: Vec<_> = c_term.free_vars().into_iter().collect();
        free_vars.sort();

        let (thunk_def_name, mut redex) = self.create_new_redex(&free_vars, *may_have_complex_effects);
        std::mem::swap(c_term, &mut redex);
        self.create_new_def(thunk_def_name, free_vars, &[], redex, *may_have_complex_effects);
    }

    fn transform_lambda(&mut self, c_term: &mut CTerm) {
        self.transform_lambda_default(c_term);
        let mut free_vars: Vec<_> = c_term.free_vars().iter().copied().collect();
        free_vars.sort();

        let CTerm::Lambda { args, may_have_complex_effects, .. } = c_term else { unreachable!() };

        let (thunk_def_name, mut redex) = self.create_new_redex(&free_vars, *may_have_complex_effects);
        let args = args.clone();
        std::mem::swap(c_term, &mut redex);

        let CTerm::Lambda { box body, may_have_complex_effects, .. } = redex else { unreachable!() };

        self.create_new_def(thunk_def_name, free_vars, &args, body, may_have_complex_effects);
    }
}

struct RedexReducer {}

impl Transformer for RedexReducer {
    fn transform_redex(&mut self, c_term: &mut CTerm) {
        self.transform_redex_default(c_term);
        let CTerm::Redex { box function, args } = c_term else { unreachable!() };
        let CTerm::Lambda { args: lambda_args, body: box lambda_body, .. } = function else { return; };
        let num_args = min(args.len(), lambda_args.len());
        let matching_args = args.drain(..num_args).collect::<Vec<_>>();
        let matching_lambda_args = lambda_args.drain(..num_args).map(|(index, _)| index).collect::<Vec<_>>();
        let mut substitutor = Substitutor { bindings: HashMap::from_iter(matching_lambda_args.into_iter().zip(matching_args)) };
        substitutor.transform_c_term(lambda_body);
        RedundancyRemover {}.transform_c_term(c_term);
        // Call the reducer again to reduce the new redex. This is terminating because any loops
        // can only be introduced through recursive calls, which are not reduced by this.
        self.transform_c_term(c_term);
    }

    fn transform_force(&mut self, c_term: &mut CTerm) {
        self.transform_force_default(c_term);
        let CTerm::Force { thunk, .. } = c_term else { unreachable!() };
        let VTerm::Thunk { box t, .. } = thunk else { return; };
        // The content of placeholder does not matter here
        let mut placeholder = CTerm::Return { value: VTerm::Var { index: 0 } };
        std::mem::swap(t, &mut placeholder);
        *c_term = placeholder;
        // Call the reducer again to reduce the new redex. This is terminating because any loops
        // can only be introduced through recursive calls, which are not reduced by this.
        self.transform_c_term(c_term);
    }
}

struct Substitutor {
    bindings: HashMap<usize, VTerm>,
}

impl Transformer for Substitutor {
    fn transform_var(&mut self, v_term: &mut VTerm) {
        let VTerm::Var { index } = v_term else { unreachable!() };
        if let Some(replacement) = self.bindings.get(index) {
            *v_term = replacement.clone();
        }
    }
}