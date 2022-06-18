mod azure;
mod settings;

use crate::azure::AzureClient;
use crate::settings::AppSettings;
use anyhow::Result;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let settings = AppSettings::fetch()?;

    let azure_client = AzureClient::from_settings(&settings)?;
    azure_client.work().await?;

    Ok(())
}

// use graph_rs_sdk::client::Graph;
// use graph_rs_sdk::oauth::OAuth;
//
// #[tokio::main(flavor = "current_thread")]
// async fn main() -> Result<()> {
//     let mut oauth = OAuth::new()
//         .client_id("efddd0e1-1704-432b-b74b-b246dbee50bf")
//         .client_secret("QXG8Q~XNDVGLK6HOmOe6vm~AeIpe3u06iI9KbaxO")
//         .tenant_id("6643a3bd-8975-46e6-a6ce-1b8025b70944")
//         .add_scope("https://graph.microsoft.com/.default")
//         .build_async()
//         .client_credentials();
//     let token = oauth.access_token().send().await?;
//     let graph_client = Graph::new_async(token.bearer_token());
//     let response = graph_client
//         .v1()
//         .applications()
//         .list_application()
//         .select(&["appId,displayName,keyCredentials,passwordCredentials"])
//         .send()
//         .await?;
//
//     let body = response.body();
//     println!(
//         "{:#?}",
//         body.as_object().unwrap()["value"].as_array().unwrap().len()
//     );
//
//     Ok(())
// }
