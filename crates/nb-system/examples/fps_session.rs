//! Tries to start the naelbar-fps ETW session once and prints the result code.
fn main() {
    match nb_system::fps::FpsSession::start(std::process::id()) {
        Ok(s) => {
            println!("started: 0, fps {:?}", s.fps());
        }
        Err(code) => println!("start failed: {code}"),
    }
}
