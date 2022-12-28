use futures::StreamExt;
use protocol::hello_world::greeter_client::GreeterClient;
use protocol::hello_world::HelloRequest;


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = GreeterClient::connect("http://[::1]:50051").await?;

    let request = tonic::Request::new(HelloRequest {
        name: "Tonic".into(),
    });

    let mut response = client.say_hello(request).await?;

    loop {
        println!("RESPONSE={:?}", response.get_mut().next().await);
    }

    //Ok(())
}
