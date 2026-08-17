#![feature(rustc_private)]

extern crate rustc_driver;

rustc_driver::override_c_allocator_in_binary!();

use rustc_codegen_cranelift::rustc_daemon::main;
