use kinode_process_lib::{call_init, net, println, Address, Message, Request};

wit_bindgen::generate!({
    path: "target/wit",
    world: "process-v0",
});

call_init!(init);
fn init(_our: Address) {
    let Ok(Ok(Message::Response { body, .. })) = Request::to(("our", "net", "distro", "sys"))
        .body(rmp_serde::to_vec(&net::NetAction::GetDiagnostics).unwrap())
        .send_and_await_response(60)
    else {
        println!("failed to get diagnostics from networking module");
        return;
    };
    let Ok(net::NetResponse::Diagnostics(printout)) = rmp_serde::from_slice(&body) else {
        println!("got malformed response from networking module");
        return;
    };
    println!("\r\n{printout}");
}
