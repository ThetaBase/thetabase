//! Generate Rust bindings from `schema/theta.capnp`.
//!
//! Compilation and code generation both run in-process: `capnpc-embedded`
//! parses the schema and `capnpc` emits Rust from the resulting IR. Neither
//! needs the `capnp` binary, so building ThetaBase does not require installing a
//! Cap'n Proto toolchain — only regenerating the SDK bindings does.

fn main() {
    println!("cargo:rerun-if-changed=schema/theta.capnp");

    let ir = capnpc_embedded::CompileCommand::new()
        .src_prefix("schema")
        .file("schema/theta.capnp")
        .compile()
        .expect("theta.capnp failed to compile");

    capnpc::codegen::CodeGenerationCommand::new()
        .output_directory(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"))
        .run(ir.as_slice())
        .expect("failed to generate Rust bindings from theta.capnp");
}
