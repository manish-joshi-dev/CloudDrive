fn main() {
    slint_build::compile("ui/app.slint").unwrap();  // single quotes → double quotes, + .unwrap()
}