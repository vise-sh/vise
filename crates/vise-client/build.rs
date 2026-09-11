use std::{fs, path::Path};

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let spec_path = Path::new(&manifest_dir).join("../../openapi/openapi.json");

    println!("cargo:rerun-if-changed={}", spec_path.display());

    let src = fs::read_to_string(&spec_path)
        .unwrap_or_else(|_| panic!("openapi/openapi.json not found — run `just gen-spec` first"));

    let spec = serde_json::from_str(&src).unwrap();
    let mut generator = progenitor::Generator::default();
    let tokens = generator.generate_tokens(&spec).unwrap();
    let ast = syn::parse2(tokens).unwrap();
    let content = prettyplease::unparse(&ast);

    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("codegen.rs");
    fs::write(out, content).unwrap();
}
