use std::cell::RefCell;
use crate::runtime::{Continuation, HandlerEntry, Handler, Uniform, ThunkPtr, Eff, Generic, CapturedContinuation};
use crate::types::{UniformPtr, UniformType};

// TODO: use custom allocator that allocates through Boehm GC for vecs
thread_local!(
    static HANDLERS: RefCell<Vec<HandlerEntry>> = RefCell::new(Vec::new());
);

pub unsafe fn runtime_alloc(num_words: usize) -> *mut usize {
    let mut vec = Vec::with_capacity(num_words + 1);
    let ptr: *mut usize = vec.as_mut_ptr();
    // Write the size of this allocation
    ptr.write(num_words);
    std::mem::forget(vec);
    ptr.add(1)
}

pub unsafe fn runtime_word_box() -> *mut usize {
    let mut vec = Vec::with_capacity(1);
    let ptr = vec.as_mut_ptr();
    std::mem::forget(vec);
    ptr
}


/// Takes a pointer to a function or thunk, push any arguments to the tip of the stack, and return
/// a pointer to the underlying raw function.
pub unsafe fn runtime_force_thunk(thunk: *const usize, tip_address_ptr: *mut *mut usize) -> *const usize {
    let thunk_ptr = thunk.to_normal_ptr();
    if UniformType::from_bits(thunk as usize) == UniformType::PPtr {
        thunk_ptr
    } else {
        let next_thunk = thunk_ptr.read() as *const usize;
        let num_args = thunk_ptr.add(1).read();
        let mut tip_address = tip_address_ptr.read();
        for i in (0..num_args).rev() {
            let arg = thunk_ptr.add(2 + i).read();
            tip_address = tip_address.sub(1);
            tip_address.write(arg);
        }
        tip_address_ptr.write(tip_address);
        runtime_force_thunk(next_thunk, tip_address_ptr)
    }
}

/// Alocate
pub unsafe fn runtime_alloc_stack() -> *mut usize {
    // Allocate a stack with 8MB of space
    let stack_size = 1 << 6;
    let mut vec: Vec<usize> = Vec::with_capacity(stack_size);
    let start: *mut usize = vec.as_mut_ptr();
    std::mem::forget(vec);
    start.add(stack_size)
}

/// Returns the result of the operation in uniform representation
/// TODO: add args
pub unsafe fn runtime_handle_simple_operation(eff: usize, base_address: *const usize) -> usize {
    let (handler_index, handler_impl, num_args) = find_matching_handler(eff, true);
    todo!()
}

/// Returns the following results on the argument stack.
/// - ptr + 0: the function pointer to the matched handler implementationon
/// - ptr + 8: the base address used to find the arguments when invoking the handler implementation
/// - ptr + 16: the next continuation after handler finishes execution
/// The pointer + 8 is the base address that should be passed to this pointed handler implementaion
/// function, which will find its arguments from that base address.
pub unsafe fn runtime_prepare_complex_operation(
    eff: usize,
    handler_call_base_address: *const usize,
    tip_continuation: &mut Continuation,
) -> *const usize {
    let (handler_index, handler_impl, tip_operation_num_args) = find_matching_handler(eff, false);
    let handler_entry_fragment = HANDLERS.with(|handler| handler.borrow_mut().split_off(handler_index));
    let matching_handler = match handler_entry_fragment.first().unwrap() {
        HandlerEntry::Handler(handler) => handler,
        _ => panic!("Expect a handler entry")
    };

    // Update the tip continuation so that its height no longer includes the arguments passed to
    // the handler because the captured continuation won't include them in the stack fragment. Later
    // when the captured continuation is resumed, the tip (tip - 8) of the argument stack will be
    // where the operation result is placed.
    tip_continuation.arg_stack_frame_height -= tip_operation_num_args;

    let matching_parameter = matching_handler.parameter;
    // Split the continuation chain at the matching handler.
    let base_continuation = matching_handler.transform_continuation;
    let next_continuation = (*base_continuation).next;
    // Tie up the captured continuation end.
    (*base_continuation).next = std::ptr::null_mut::<Continuation>();

    // Update the next continuation to make it ready for calling the handler implementation
    // Plus 2 for handler parameter and reified continuation
    (*next_continuation).arg_stack_frame_height += tip_operation_num_args + 2 - matching_handler.transform_num_args;

    // Copy the stack fragment.
    let stack_fragment_end = matching_handler.transform_base_address.add(matching_handler.transform_num_args);
    let stack_fragment_start = handler_call_base_address.add(tip_operation_num_args);
    let stack_fragment_length = stack_fragment_end.offset_from(stack_fragment_start);
    assert!(stack_fragment_length >= 0);
    let stack_fragment: Vec<Generic> = std::slice::from_raw_parts(stack_fragment_start, stack_fragment_length as usize).to_vec();

    // Copy the handler fragment.
    let base_offset = matching_handler.transform_base_address as usize;
    let handler_fragment: Vec<Handler<usize>> = handler_entry_fragment.into_iter().map(|handler_entry| {
        match handler_entry {
            HandlerEntry::Handler(handler) => {
                let mut handler: Handler<usize> = std::mem::transmute(handler);
                handler.transform_base_address -= base_offset;
                handler
            }
            _ => panic!("Expect a handler entry")
        }
    }).collect();

    let captured_continuation = runtime_alloc((std::mem::size_of::<CapturedContinuation>()) / 8) as *mut CapturedContinuation;

    *captured_continuation = CapturedContinuation {
        tip_continuation,
        base_continuation,
        handler_fragment,
        stack_fragment,
    };

    let mut new_tip_address = stack_fragment_end as *mut usize;
    // Set up arguments for invoking the handler.
    // First set up the handler parameter.
    new_tip_address = new_tip_address.sub(1);
    new_tip_address.write(matching_parameter);

    // Then set up explicit handler arguments.
    for i in (0..tip_operation_num_args).rev() {
        new_tip_address = new_tip_address.sub(1);
        new_tip_address.write(handler_call_base_address.add(i).read());
    }

    // Lastly, set up the reified continuation.
    new_tip_address = new_tip_address.sub(1);
    new_tip_address.write(UniformType::to_uniform_sptr(captured_continuation));

    // Set up return values.
    let handler_function_ptr = runtime_force_thunk(handler_impl, &mut new_tip_address);
    let new_base_address = new_tip_address;
    let result_ptr = new_tip_address.add(3);
    result_ptr.write(handler_function_ptr as usize);
    result_ptr.add(1).write(new_base_address as usize);
    result_ptr.add(2).write(next_continuation as usize);
    result_ptr
}

pub unsafe fn runtime_pop_handler() -> Uniform {
    HANDLERS.with(|handlers| {
        let handler = handlers.borrow_mut().pop().unwrap();
        match handler {
            HandlerEntry::Handler(handler) => handler.parameter,
            _ => panic!("Expect a handler entry")
        }
    })
}

fn find_matching_handler(eff: Eff, simple: bool) -> (usize, ThunkPtr, usize) {
    HANDLERS.with(|handler| {
        for (i, e) in handler.borrow().iter().rev().enumerate() {
            if let HandlerEntry::Handler(handler) = e {
                if simple {
                    for (e, handler_impl, num_args) in &handler.simple_handler {
                        if unsafe { compare_uniform(eff, *e) } {
                            return (i, *handler_impl, *num_args);
                        }
                    }
                } else {
                    for (e, handler_impl, num_args) in &handler.complex_handler {
                        if unsafe { compare_uniform(eff, *e) } {
                            return (i, *handler_impl, *num_args);
                        }
                    }
                }
            }
        }
        panic!("No matching handler found")
    })
}


pub unsafe fn runtime_register_handler_and_get_transform_continuation(
    caller_tip_address: *mut usize,
    caller_continuation: &mut Continuation,
    parameter: Uniform,
    parameter_disposer: ThunkPtr,
    parameter_replicator: ThunkPtr,
    transform: ThunkPtr,
    transform_num_args: usize,
    transform_var_bound: usize,
) -> *const Continuation {
    let transform_continuation = &mut *(runtime_alloc(4 + transform_var_bound) as *mut Continuation);
    let mut tip_address = caller_tip_address;
    transform_continuation.func = runtime_force_thunk(transform, &mut tip_address);
    transform_continuation.next = caller_continuation;
    transform_continuation.arg_stack_frame_height = 0;
    transform_continuation.state = 0;

    // This is needed because by joining transform continuation with the caller continuation, we
    // are essentially making the caller function call the transform function with the needed
    // arguments. Hence the arg stack frame height of the caller continuation should be increased
    // by the number of arguments needed by the transform function.
    // Note that the parameter and result arguments of the transform function are obtained via
    // special constructs `PopHandler`, and `GetLastResult` rather than the argument stack. These
    // special constructs are created during signature optimization, which, in addition, also lifts
    // all the components of a handler.
    caller_continuation.arg_stack_frame_height += ((tip_address as usize) - (caller_tip_address as usize) / 8);

    HANDLERS.with(|handlers| handlers.borrow_mut().push(HandlerEntry::Handler(Handler {
        transform_base_address: tip_address,
        transform_continuation,
        transform_num_args,
        parameter,
        parameter_disposer,
        parameter_replicator,
        // TODO: use custom allocator that allocates through Boehm GC for vecs
        simple_handler: Vec::new(),
        complex_handler: Vec::new(),
    })));
    transform_continuation
}

pub unsafe fn runtime_get_current_handler() -> *mut Handler<*const usize> {
    HANDLERS.with(|handlers| {
        let mut handlers = handlers.borrow_mut();
        let handler = handlers.last_mut().unwrap();
        match handler {
            HandlerEntry::Handler(handler) => handler as *mut Handler<*const usize>,
            _ => panic!("Expect a handler entry")
        }
    })
}

pub fn runtime_add_simple_handler(handler: &mut Handler<*const usize>, eff: Eff, handler_impl: ThunkPtr, num_args: usize) {
    handler.simple_handler.push((eff, handler_impl, num_args))
}

pub fn runtime_add_complex_handler(handler: &mut Handler<*const usize>, eff: Eff, handler_impl: ThunkPtr, num_args: usize) {
    handler.complex_handler.push((eff, handler_impl, num_args))
}

/// Returns a pointer pointing to the following:
/// - ptr + 0: the function pointer to the resumed continuation implementation
/// - ptr + 8: the base address for the resumed continuation to find its arguments
/// - ptr + 16: the pointer to the "last result" that should be passed to the resumed continuation
pub unsafe fn runtime_prepare_resume_continuation(
    mut base_address: *mut usize,
    captured_continuation: *mut CapturedContinuation,
    parameter: Uniform,
    result: Uniform,
    next_continuation: &mut Continuation,
) -> *const usize {
    let captured_continuation = captured_continuation.read();
    let base_handler = captured_continuation.handler_fragment.first().unwrap();
    (*captured_continuation.base_continuation).next = next_continuation;
    // Chain the base of the captured continuation to the next continuation, where we need to add
    // all the arguments for the handler transform function to the argument stack. Hence we need to
    // update the stack frame height of the next continuation.
    next_continuation.arg_stack_frame_height += base_handler.transform_num_args;

    let transform_base_address = base_address.add(base_handler.transform_num_args);
    for arg in captured_continuation.stack_fragment.iter().rev() {
        base_address = base_address.sub(1);
        base_address.write(*arg);
    }
    let new_base_address = base_address;

    HANDLERS.with(|handlers| {
        let mut handlers = handlers.borrow_mut();
        for handler in captured_continuation.handler_fragment.into_iter() {
            handlers.push(HandlerEntry::Handler(Handler {
                transform_base_address: transform_base_address.add(handler.transform_base_address),
                transform_continuation: handler.transform_continuation,
                transform_num_args: handler.transform_num_args,
                parameter: handler.parameter,
                parameter_disposer: handler.parameter_disposer,
                parameter_replicator: handler.parameter_replicator,
                simple_handler: handler.simple_handler,
                complex_handler: handler.complex_handler,
            }));
        }
    });

    // Write the last result
    let last_result_address = base_address.add(1);
    last_result_address.write(result);

    // Write the return values of this helper function.
    base_address = last_result_address.add(3);
    base_address.write((*captured_continuation.tip_continuation).func as usize);
    base_address.add(1).write(new_base_address as usize);
    base_address.add(2).write(last_result_address as usize);

    base_address
}

unsafe fn compare_uniform(a: Uniform, b: Uniform) -> bool {
    if a == b {
        return true;
    }
    let a_type = UniformType::from_bits(a);
    let b_type = UniformType::from_bits(b);
    if a_type != b_type {
        return false;
    }
    match a_type {
        UniformType::Raw => false,
        UniformType::SPtr => {
            let a_ptr = a.to_normal_ptr();
            let b_ptr = b.to_normal_ptr();
            let a_size = a_ptr.sub(8).read();
            let b_size = b_ptr.sub(8).read();
            if a_size != b_size {
                return false;
            }
            for i in 0..a_size {
                if !compare_uniform(a_ptr.add(i).read(), b_ptr.add(i).read()) {
                    return false;
                }
            }
            true
        }
        UniformType::PPtr => {
            let a_ptr = a.to_normal_ptr();
            let b_ptr = b.to_normal_ptr();
            let a_size = a_ptr.sub(8).read();
            let b_size = b_ptr.sub(8).read();
            if a_size != b_size {
                return false;
            }
            for i in 0..a_size {
                if a_ptr.add(i).read() != b_ptr.add(i).read() {
                    return false;
                }
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        unsafe {
            let ptr = runtime_alloc(10);
            *ptr = 42;
            assert_eq!(ptr as usize % 8, 0);
            assert_eq!(ptr.read(), 42);
        }
    }
}
