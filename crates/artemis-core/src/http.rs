use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use openssl::pkey::{PKeyRef, Private};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use quick_xml::Reader;
use quick_xml::events::Event;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{ClientIdentity, Error, HostAddress, Result, ServerInfo};

const DEFAULT_HTTPS_PORT: u16 = 47_984;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(7);

/// `GameStream` HTTP client with mutual TLS and exact server-certificate pinning.
pub struct NvClient {
    address: HostAddress,
    identity: ClientIdentity,
    https_port: u16,
    pinned_certificate: Option<Vec<u8>>,
}

impl NvClient {
    #[must_use]
    pub fn new(
        address: HostAddress,
        identity: ClientIdentity,
        https_port: Option<u16>,
        pinned_certificate: Option<Vec<u8>>,
    ) -> Self {
        Self {
            address,
            identity,
            https_port: https_port.unwrap_or(DEFAULT_HTTPS_PORT),
            pinned_certificate,
        }
    }

    #[must_use]
    pub fn address(&self) -> &HostAddress {
        &self.address
    }

    #[must_use]
    pub fn https_port(&self) -> u16 {
        self.https_port
    }

    pub fn set_https_port(&mut self, port: u16) {
        self.https_port = port;
    }

    pub fn set_pinned_certificate(&mut self, certificate: Vec<u8>) {
        self.pinned_certificate = Some(certificate);
    }

    #[must_use]
    pub fn pinned_certificate(&self) -> Option<&[u8]> {
        self.pinned_certificate.as_deref()
    }

    pub(crate) fn identity_certificate_pem(&self) -> Result<Vec<u8>> {
        self.identity.certificate_pem()
    }

    pub(crate) fn identity_certificate_signature(&self) -> &[u8] {
        self.identity.certificate().signature().as_slice()
    }

    pub(crate) fn identity_private_key(&self) -> &PKeyRef<Private> {
        self.identity.private_key()
    }

    /// Fetches `/serverinfo`, using pinned HTTPS once paired.
    ///
    /// # Errors
    ///
    /// Returns an error for connection, TLS pin, HTTP, XML, or mandatory-field failures.
    pub fn server_info(&mut self) -> Result<ServerInfo> {
        let xml = if self.pinned_certificate.is_some() {
            self.request_https("serverinfo", &[], false)?
        } else {
            self.request_http("serverinfo", &[], false)?
        };
        let document = XmlDocument::parse(&xml)?;
        let server_info = ServerInfo {
            name: document
                .optional("hostname")
                .unwrap_or("Unknown host")
                .to_owned(),
            unique_id: document.required("uniqueid")?.to_owned(),
            app_version: document.required("appversion")?.to_owned(),
            gfe_version: document.optional("GfeVersion").map(str::to_owned),
            pair_status: document.optional("PairStatus") == Some("1"),
            https_port: document
                .optional("HttpsPort")
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_HTTPS_PORT),
            current_game: document
                .optional("currentgame")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            codec_mode_support: document
                .optional("ServerCodecModeSupport")
                .and_then(|value| value.parse().ok())
                .unwrap_or(1),
            state: document.optional("state").unwrap_or_default().to_owned(),
        };
        self.https_port = server_info.https_port;
        Ok(server_info)
    }

    pub(crate) fn request_http(
        &self,
        path: &str,
        parameters: &[(&str, String)],
        wait_for_user: bool,
    ) -> Result<String> {
        self.request(path, parameters, Transport::Http, wait_for_user)
    }

    pub(crate) fn request_https(
        &self,
        path: &str,
        parameters: &[(&str, String)],
        wait_for_user: bool,
    ) -> Result<String> {
        self.request(path, parameters, Transport::Https, wait_for_user)
    }

    fn request(
        &self,
        path: &str,
        parameters: &[(&str, String)],
        transport: Transport,
        wait_for_user: bool,
    ) -> Result<String> {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (name, value) in parameters {
            serializer.append_pair(name, value);
        }
        serializer.append_pair("devicename", "Artemis Linux");
        serializer.append_pair("uniqueid", self.identity.unique_id());
        serializer.append_pair("uuid", &Uuid::new_v4().to_string());
        let query = serializer.finish();
        let request_path = format!("/{path}?{query}");

        let port = match transport {
            Transport::Http => self.address.http_port,
            Transport::Https => self.https_port,
        };
        let stream = connect(&self.address.host, port, wait_for_user)?;
        let response = match transport {
            Transport::Http => exchange(stream, &self.address.host, &request_path)?,
            Transport::Https => {
                let pin = self.pinned_certificate.as_deref().ok_or_else(|| {
                    Error::Configuration(
                        "a pinned server certificate is required for HTTPS".to_owned(),
                    )
                })?;
                let mut builder = SslConnector::builder(SslMethod::tls_client())?;
                builder.set_certificate(self.identity.certificate())?;
                builder.set_private_key(self.identity.private_key())?;
                builder.check_private_key()?;

                // Certificate validation is performed immediately after the handshake using
                // an exact DER pin. Host certificates are normally self-signed and addressed
                // by IP, so platform PKI and hostname validation cannot establish identity.
                builder.set_verify(SslVerifyMode::NONE);
                let connector = builder.build();
                let mut tls = connector
                    .connect(&self.address.host, stream)
                    .map_err(|error| Error::Http(format!("TLS handshake failed: {error}")))?;
                let peer = tls
                    .ssl()
                    .peer_certificate()
                    .ok_or_else(|| Error::Http("TLS peer supplied no certificate".to_owned()))?;
                if peer.to_der()?.as_slice() != pin {
                    return Err(Error::Http(
                        "TLS peer certificate does not match the paired host pin".to_owned(),
                    ));
                }
                exchange(&mut tls, &self.address.host, &request_path)?
            }
        };

        parse_http_response(&response)
    }
}

#[derive(Clone, Copy)]
enum Transport {
    Http,
    Https,
}

fn connect(host: &str, port: u16, wait_for_user: bool) -> Result<TcpStream> {
    let addresses = (host, port).to_socket_addrs()?;
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                stream.set_write_timeout(Some(READ_TIMEOUT))?;
                stream.set_read_timeout((!wait_for_user).then_some(READ_TIMEOUT))?;
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.map_or_else(
        || Error::Http(format!("no addresses resolved for {host}")),
        Error::Io,
    ))
}

fn exchange<S>(mut stream: S, host: &str, path: &str) -> Result<Vec<u8>>
where
    S: Read + Write,
{
    let host_header = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         User-Agent: Artemis-Linux/0.1\r\n\
         Accept: application/xml\r\n\
         Accept-Encoding: identity\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut response = Vec::with_capacity(8 * 1024);
    stream.read_to_end(&mut response)?;
    Ok(response)
}

fn parse_http_response(response: &[u8]) -> Result<String> {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| Error::Http("HTTP response contained no header terminator".to_owned()))?;
    let header = std::str::from_utf8(&response[..separator])
        .map_err(|error| Error::Http(format!("HTTP header was not UTF-8: {error}")))?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| Error::Http("HTTP response contained no valid status".to_owned()))?;
    if !(200..300).contains(&status) {
        return Err(Error::HostStatus {
            code: i64::from(status),
            message: "HTTP request failed".to_owned(),
        });
    }

    let mut body = response[(separator + 4)..].to_vec();
    if header.lines().any(|line| {
        line.eq_ignore_ascii_case("transfer-encoding: chunked")
            || line
                .to_ascii_lowercase()
                .starts_with("transfer-encoding: chunked")
    }) {
        body = decode_chunked(&body)?;
    }
    String::from_utf8(body)
        .map_err(|error| Error::Http(format!("HTTP response body was not UTF-8: {error}")))
}

fn decode_chunked(body: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(body.len());
    let mut cursor = 0;
    loop {
        let line_end = body[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|offset| cursor + offset)
            .ok_or_else(|| Error::Http("malformed chunked response".to_owned()))?;
        let size_text = std::str::from_utf8(&body[cursor..line_end])
            .map_err(|error| Error::Http(format!("invalid chunk size: {error}")))?;
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16)
                .map_err(|error| Error::Http(format!("invalid chunk size: {error}")))?;
        cursor = line_end + 2;
        if size == 0 {
            break;
        }
        let end = cursor
            .checked_add(size)
            .filter(|end| *end <= body.len())
            .ok_or_else(|| Error::Http("chunk exceeds response length".to_owned()))?;
        decoded.extend_from_slice(&body[cursor..end]);
        cursor = end + 2;
        if cursor > body.len() {
            return Err(Error::Http("malformed chunk delimiter".to_owned()));
        }
    }
    Ok(decoded)
}

#[derive(Debug)]
pub(crate) struct XmlDocument {
    fields: Vec<(String, String)>,
}

impl XmlDocument {
    pub(crate) fn parse(xml: &str) -> Result<Self> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut buffer = Vec::new();
        let mut stack = Vec::<String>::new();
        let mut fields = Vec::new();
        let mut status = None;
        let mut status_message = String::new();

        loop {
            match reader.read_event_into(&mut buffer)? {
                Event::Start(event) => {
                    let name = String::from_utf8_lossy(event.name().as_ref()).into_owned();
                    if name == "root" {
                        for attribute in event.attributes().flatten() {
                            let key = String::from_utf8_lossy(attribute.key.as_ref()).into_owned();
                            let value =
                                String::from_utf8_lossy(attribute.value.as_ref()).into_owned();
                            if key == "status_code" {
                                status = value.parse::<i64>().ok();
                            } else if key == "status_message" {
                                status_message = value;
                            }
                        }
                    }
                    stack.push(name);
                }
                Event::Text(event) => {
                    if let Some(name) = stack.last() {
                        let value = event.unescape()?.into_owned();
                        fields.push((name.clone(), value));
                    }
                }
                Event::End(_) => {
                    stack.pop();
                }
                Event::Eof => break,
                _ => {}
            }
            buffer.clear();
        }

        if let Some(code) = status {
            if code != 200 {
                return Err(Error::HostStatus {
                    code,
                    message: status_message,
                });
            }
        }
        Ok(Self { fields })
    }

    pub(crate) fn required(&self, name: &str) -> Result<&str> {
        self.optional(name)
            .ok_or_else(|| Error::InvalidResponse(format!("missing mandatory XML field `{name}`")))
    }

    pub(crate) fn optional(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find_map(|(field, value)| (field == name).then_some(value.as_str()))
    }

    pub(crate) fn all(&self, name: &str) -> impl Iterator<Item = &str> {
        self.fields
            .iter()
            .filter_map(move |(field, value)| (field == name).then_some(value.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::{XmlDocument, decode_chunked, parse_http_response};

    #[test]
    fn parses_xml_status_and_fields() {
        let document = XmlDocument::parse(
            r#"<root status_code="200" status_message="OK"><hostname>Lab</hostname></root>"#,
        )
        .expect("valid response");
        assert_eq!(document.required("hostname").expect("hostname"), "Lab");
    }

    #[test]
    fn rejects_host_xml_error() {
        let error =
            XmlDocument::parse(r#"<root status_code="401" status_message="Not paired"></root>"#)
                .expect_err("status should fail");
        assert!(error.to_string().contains("401"));
    }

    #[test]
    fn decodes_chunked_body() {
        assert_eq!(
            decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").expect("chunked"),
            b"Wikipedia"
        );
    }

    #[test]
    fn parses_http_body() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
        assert_eq!(parse_http_response(response).expect("response"), "OK");
    }
}
