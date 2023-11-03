#![feature(trait_alias)]
#![feature(box_patterns)]
#![allow(dead_code)]

mod term;
mod signature;
mod visitor;
mod u_term;
mod transpiler;
mod parser;
mod test_utils;
mod free_var;
mod compiler;
mod transformer;

fn main() {
    println!("Hello, world!");
}