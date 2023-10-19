use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum UTerm {
    Identifier { name: String },
    Int { value: i32 },
    Str { value: String },
    Tuple { values: Vec<UTerm> },
    Lambda { arg_names: Vec<String>, body: Box<UTerm> },
    Thunk { t: Box<UTerm> },
    App { function: Box<UTerm>, args: Vec<UTerm> },
    Force { thunk: Box<UTerm> },
    CaseInt { t: Box<UTerm>, branches: HashMap<i32, UTerm>, default_branch: Option<Box<UTerm>> },
    CaseStr { t: Box<UTerm>, branches: HashMap<String, UTerm>, default_branch: Option<Box<UTerm>> },
    CaseTuple { t: Box<UTerm>, names: Vec<String>, branch: Box<UTerm> },
    Let { name: String, t: Box<UTerm>, body: Box<UTerm> },
    Defs { defs: HashMap<String, Def>, body: Option<Box<UTerm>> },
}

#[derive(Debug, Clone)]
pub struct Def {
    pub args: Vec<String>,
    pub block: Box<UTerm>,
}