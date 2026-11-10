//! Generates the C# P/Invoke declarations from the C ABI (§0, §10.4).

fn main() {
    println!("cargo:rerun-if-changed=src/abi.rs");
    csbindgen::Builder::default()
        .input_extern_file("src/abi.rs")
        .csharp_dll_name("nexa_client")
        .csharp_namespace("NexaDesk.Native")
        .csharp_class_name("NativeMethods")
        .csharp_class_accessibility("public")
        .csharp_generate_const_filter(|name| name.starts_with("NEXA_"))
        .generate_csharp_file("bindings/NativeMethods.g.cs")
        .expect("generate C# bindings");
}
