use std::env;
use std::fs;

fn main() {
    println!("hello embedded wasm");
    if let Ok(val) = env::var("EMBED_VAR") {
        println!("env:EMBED_VAR={}", val);
    }
    if let Ok(content) = fs::read_to_string("/data/test.txt") {
        print!("file:{}", content);
    }
}
