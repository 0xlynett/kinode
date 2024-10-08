use crate::kinode::process::main::{
    InstallPackageRequest, InstallResponse, LocalRequest, LocalResponse,
};
use kinode_process_lib::{
    await_next_message_body, call_init, println, Address, Message, PackageId, Request,
};

wit_bindgen::generate!({
    path: "target/wit",
    generate_unused_types: true,
    world: "app-store-sys-v0",
    additional_derives: [PartialEq, serde::Deserialize, serde::Serialize],
});

call_init!(init);
fn init(our: Address) {
    let Ok(body) = await_next_message_body() else {
        println!("install: failed to get args!");
        return;
    };

    let arg = String::from_utf8(body).unwrap_or_default();
    let args: Vec<&str> = arg.split_whitespace().collect();

    if args.len() != 2 {
        println!(
            "install: 2 arguments required, the package id of the app and desired version_hash"
        );
        println!("example: install app:publisher.os f5d374ab50e66888a7c2332b22d0f909f2e3115040725cfab98dcae488916990");
        return;
    }

    let Ok(package_id) = args[0].parse::<PackageId>() else {
        println!("install: invalid package id, make sure to include package name and publisher");
        println!("example: app_name:publisher_name");
        return;
    };

    let version_hash = args[1].to_string();

    let Ok(Ok(Message::Response { body, .. })) =
        Request::to((our.node(), ("main", "app_store", "sys")))
            .body(
                serde_json::to_vec(&LocalRequest::Install(InstallPackageRequest {
                    package_id: crate::kinode::process::main::PackageId {
                        package_name: package_id.package_name.clone(),
                        publisher_node: package_id.publisher_node.clone(),
                    },
                    version_hash,
                    metadata: None,
                }))
                .unwrap(),
            )
            .send_and_await_response(5)
    else {
        println!("install: failed to get a response from app_store..!");
        return;
    };

    let Ok(response) = serde_json::from_slice::<LocalResponse>(&body) else {
        println!("install: failed to parse response from app_store..!");
        return;
    };

    match response {
        LocalResponse::InstallResponse(InstallResponse::Success) => {
            println!("successfully installed package {package_id}");
        }
        LocalResponse::InstallResponse(InstallResponse::Failure) => {
            println!("failed to install package {package_id}");
            println!("make sure that the package has been downloaded!")
        }
        _ => {
            println!("install: unexpected response from app_store..!");
            return;
        }
    }
}
