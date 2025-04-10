use reqwest::Url;

#[derive(Debug)]
pub struct Client {
    pub url: Url,
    pub client: reqwest::Client,
}