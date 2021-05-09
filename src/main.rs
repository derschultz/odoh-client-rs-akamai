pub mod config;
pub mod dns_utils;
use anyhow::{anyhow, Context, Result};
use clap::{App, Arg};
use config::Config;
use dns_utils::{create_dns_query, parse_dns_answer};
use odoh_rs::protocol::{
    create_query_msg, get_supported_config, parse_received_response, ObliviousDoHConfigContents,
    ObliviousDoHQueryBody, ODOH_HTTP_HEADER,
};
use reqwest::{
    header::{HeaderMap, ACCEPT, CACHE_CONTROL, CONTENT_TYPE},
    Client, Response, StatusCode,
};
use std::env;
use url::Url;
use std::fs::File;
use std::io::Read;

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const PKG_AUTHORS: &str = env!("CARGO_PKG_AUTHORS");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

const QUERY_PATH: &str = "/dns-query";

#[derive(Clone, Debug)]
struct ClientSession {
    pub client: Client,
    pub target: Url,
    pub proxy: Option<Url>,
    pub client_secret: Option<Vec<u8>>,
    pub target_config: ObliviousDoHConfigContents,
    pub query: Option<ObliviousDoHQueryBody>,
}

impl ClientSession {
    /// Create a new ClientSession
    pub async fn new(config: Config, rcfile: &str) -> Result<Self> {
        let mut target = Url::parse(&config.server.target)?;
        target.set_path(QUERY_PATH);
        let proxy = if let Some(p) = &config.server.proxy {
            Url::parse(p).ok()
        } else {
            None
        };

        // instead of pulling the resolver kem/kdc/aead/public key bytes from
        // an http endpoint, as in the original code, read them from disk.
        let mut fhandle = File::open(rcfile)?;
        let mut filevec = Vec::new();
        let _count = fhandle.read_to_end(&mut filevec);
        let bytes = filevec.as_slice();
        let target_config = get_supported_config(&bytes)?;

        Ok(Self {
            client: Client::new(),
            target,
            proxy,
            client_secret: None,
            target_config,
            query: None,
        })
    }

    /// Create an oblivious query from a domain and query type
    pub fn create_request(&mut self, domain: &str, qtype: &str) -> Result<Vec<u8>> {
        // create a DNS message
        let dns_msg = create_dns_query(domain, qtype)?;
        let query = ObliviousDoHQueryBody::new(&dns_msg, Some(1));
        self.query = Some(query.clone());
        let (oblivious_query, client_secret) = create_query_msg(&self.target_config, &query)?;
        self.client_secret = Some(client_secret);
        Ok(oblivious_query)
    }

    /// Set headers and build an HTTP request to send the oblivious query to the proxy/target.
    /// If a proxy is specified, the request will be sent to the proxy. However, if a proxy is absent,
    /// it will be sent directly to the target. Note that not specifying a proxy effectively nullifies
    /// the entire purpose of using ODoH.
    pub async fn send_request(&mut self, request: &[u8]) -> Result<Response> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, ODOH_HTTP_HEADER.parse()?);
        headers.insert(ACCEPT, ODOH_HTTP_HEADER.parse()?);
        headers.insert(CACHE_CONTROL, "no-cache, no-store".parse()?);
        let query = [
            (
                "targethost",
                self.target
                    .host_str()
                    .context("Target host is not a valid host string")?,
            ),
            ("targetpath", QUERY_PATH),
        ];
        let builder = if let Some(p) = &self.proxy {
            self.client.post(p.clone()).headers(headers).query(&query)
        } else {
            self.client.post(self.target.clone()).headers(headers)
        };
        let resp = builder.body(request.to_vec()).send().await?;
        Ok(resp)
    }

    /// Parse the received response from the resolver and print the answer.
    pub async fn parse_response(&self, resp: Response) -> Result<()> {
        if resp.status() != StatusCode::OK {
            return Err(anyhow!(
                "query failed with response status code {}",
                resp.status().as_u16()
            ));
        }
        let data = resp.bytes().await?;
        let response_body = parse_received_response(
            &self.client_secret.clone().unwrap(),
            &data,
            &self.query.clone().unwrap(),
        )?;
        parse_dns_answer(&response_body.dns_msg)?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let matches = App::new(PKG_NAME)
        .version(PKG_VERSION)
        .author(PKG_AUTHORS)
        .about(PKG_DESCRIPTION)
        .arg(
            Arg::with_name("config_file")
                .short("c")
                .long("config")
                .value_name("FILE")
                .help("Path to the config.toml config file")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("domain")
                .help("Domain to query")
                .required(true)
                .index(1),
        )
        .arg(
            Arg::with_name("type")
                .help("Query type")
                .required(true)
                .index(2),
        )
        .arg(
            Arg::with_name("resolverconfigfile")
                .help("File containing target resolver kem/kdc/aead/public key in binary")
                .required(true)
                .index(3),
        )
        .get_matches();

    let config_file = matches
        .value_of("config_file")
        .unwrap_or("tests/config.toml");
    let config = Config::from_path(config_file)?;
    let domain = matches.value_of("domain").unwrap();
    let qtype = matches.value_of("type").unwrap();
    let rcfile = matches.value_of("resolverconfigfile").unwrap();
    let mut session = ClientSession::new(config.clone(), &rcfile).await?;
    let request = session.create_request(domain, qtype)?;
    let response = session.send_request(&request).await?;
    session.parse_response(response).await?;
    Ok(())
}
