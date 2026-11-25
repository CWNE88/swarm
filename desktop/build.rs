fn main() {
    let tests = std::env::var_os("CARGO_FEATURE_UI_TESTS").is_some();
    let config = slint_build::CompilerConfiguration::new().with_debug_info(tests);
    slint_build::compile_with_config("ui/app.slint", config).expect("compiling the Slint UI");
}
