use openssl::hash::{MessageDigest, hash};
use openssl::memcmp;
use openssl::pkey::{PKeyRef, Private};
use openssl::sign::{Signer, Verifier};
use openssl::symm::{Cipher, Crypter, Mode};
use openssl::x509::X509;
use zeroize::Zeroizing;

use crate::http::XmlDocument;
use crate::{Error, NvClient, Result, ServerInfo};

/// Terminal result of the PIN pairing exchange.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PairingOutcome {
    Paired { certificate_der: Vec<u8> },
    IncorrectPin,
    AlreadyInProgress,
}

/// Generates the four-digit PIN shown to the user during pairing.
///
/// # Errors
///
/// Returns an error if the operating system random generator fails.
pub fn generate_pin() -> Result<String> {
    let mut digits = [0_u8; 4];
    openssl::rand::rand_bytes(&mut digits)?;
    Ok(digits
        .into_iter()
        .map(|digit| char::from(b'0' + digit % 10))
        .collect())
}

/// Performs the `GameStream` PIN challenge-response protocol.
///
/// # Errors
///
/// Returns an error for malformed input, transport or XML failures, rejected protocol
/// stages, invalid signatures, or a TLS pin mismatch.
pub fn pair(
    client: &mut NvClient,
    server_info: &ServerInfo,
    pin: &str,
    passphrase: Option<&str>,
) -> Result<PairingOutcome> {
    if pin.len() != 4 || !pin.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Pairing(
            "the pairing PIN must contain exactly four digits".to_owned(),
        ));
    }
    let generation = server_info
        .app_version
        .split('.')
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| Error::Pairing("host appversion is malformed".to_owned()))?;
    let digest = if generation >= 7 {
        MessageDigest::sha256()
    } else {
        MessageDigest::sha1()
    };

    let mut salt = [0_u8; 16];
    openssl::rand::rand_bytes(&mut salt)?;
    let salt_hex = hex::encode_upper(salt);
    let mut salted_pin = Vec::with_capacity(salt.len() + pin.len());
    salted_pin.extend_from_slice(&salt);
    salted_pin.extend_from_slice(pin.as_bytes());
    let key_hash = hash(digest, &salted_pin)?;
    let mut key = Zeroizing::new([0_u8; 16]);
    key.copy_from_slice(&key_hash[..16]);

    let certificate_pem = client_certificate_hex(client)?;
    let mut initial_parameters = vec![
        ("updateState", "1".to_owned()),
        ("phrase", "getservercert".to_owned()),
        ("salt", salt_hex.clone()),
        ("clientcert", certificate_pem),
    ];
    if let Some(passphrase) = passphrase {
        let authentication = hash(
            MessageDigest::sha256(),
            format!("{pin}{salt_hex}{passphrase}").as_bytes(),
        )?;
        initial_parameters.push(("otpauth", hex::encode_upper(authentication)));
    }

    let initial_xml = client.request_http("pair", &initial_parameters, true)?;
    let initial = XmlDocument::parse(&initial_xml)?;
    require_paired(&initial)?;
    let Some(plain_certificate) = initial.optional("plaincert") else {
        let _ = client.request_http("unpair", &[], false);
        return Ok(PairingOutcome::AlreadyInProgress);
    };
    let certificate_der = hex::decode(plain_certificate)
        .map_err(|error| Error::Pairing(format!("invalid host certificate encoding: {error}")))?;
    let server_certificate = X509::from_der(&certificate_der)?;
    client.set_pinned_certificate(certificate_der.clone());

    let result = complete_pairing(client, digest, &key, &server_certificate, &certificate_der);
    if result.is_err() {
        let _ = client.request_http("unpair", &[], false);
    }
    result
}

fn complete_pairing(
    client: &NvClient,
    digest: MessageDigest,
    key: &[u8; 16],
    server_certificate: &X509,
    certificate_der: &[u8],
) -> Result<PairingOutcome> {
    let mut random_challenge = Zeroizing::new([0_u8; 16]);
    openssl::rand::rand_bytes(random_challenge.as_mut())?;
    let encrypted_challenge = aes_ecb(random_challenge.as_ref(), key, Mode::Encrypt)?;
    let challenge_xml = client.request_http(
        "pair",
        &[
            ("updateState", "1".to_owned()),
            ("clientchallenge", hex::encode_upper(encrypted_challenge)),
        ],
        false,
    )?;
    let challenge = XmlDocument::parse(&challenge_xml)?;
    require_paired(&challenge)?;
    let challenge_response = decode_field(&challenge, "challengeresponse")?;
    let decrypted_response = Zeroizing::new(aes_ecb(&challenge_response, key, Mode::Decrypt)?);
    let hash_length = digest.size();
    if decrypted_response.len() < hash_length + 16 {
        return Err(Error::Pairing(
            "host challenge response was truncated".to_owned(),
        ));
    }
    let server_response = &decrypted_response[..hash_length];
    let server_challenge = &decrypted_response[hash_length..hash_length + 16];

    let mut client_secret = Zeroizing::new([0_u8; 16]);
    openssl::rand::rand_bytes(client_secret.as_mut())?;
    let mut response_material = Zeroizing::new(Vec::new());
    response_material.extend_from_slice(server_challenge);
    response_material.extend_from_slice(client_certificate_signature(client));
    response_material.extend_from_slice(client_secret.as_ref());
    let response_hash = hash(digest, &response_material)?;
    let encrypted_response = aes_ecb(response_hash.as_ref(), key, Mode::Encrypt)?;

    let secret_xml = client.request_http(
        "pair",
        &[
            ("updateState", "1".to_owned()),
            ("serverchallengeresp", hex::encode_upper(encrypted_response)),
        ],
        false,
    )?;
    let secret_document = XmlDocument::parse(&secret_xml)?;
    require_paired(&secret_document)?;
    let signed_server_secret = Zeroizing::new(decode_field(&secret_document, "pairingsecret")?);
    if signed_server_secret.len() <= 16 {
        return Err(Error::Pairing(
            "host pairing secret was truncated".to_owned(),
        ));
    }
    let (server_secret, server_signature) = signed_server_secret.split_at(16);
    verify_signature(server_certificate, server_secret, server_signature)?;

    let mut expected_material = Zeroizing::new(Vec::new());
    expected_material.extend_from_slice(random_challenge.as_ref());
    expected_material.extend_from_slice(server_certificate.signature().as_slice());
    expected_material.extend_from_slice(server_secret);
    let expected_response = hash(digest, &expected_material)?;
    if !memcmp::eq(expected_response.as_ref(), server_response) {
        let _ = client.request_http("unpair", &[], false);
        return Ok(PairingOutcome::IncorrectPin);
    }

    let signature = sign(client_identity_key(client), client_secret.as_ref())?;
    let mut client_pairing_secret = Zeroizing::new(Vec::with_capacity(16 + signature.len()));
    client_pairing_secret.extend_from_slice(client_secret.as_ref());
    client_pairing_secret.extend_from_slice(&signature);
    let completion_xml = client.request_http(
        "pair",
        &[
            ("updateState", "1".to_owned()),
            (
                "clientpairingsecret",
                hex::encode_upper(client_pairing_secret),
            ),
        ],
        false,
    )?;
    require_paired(&XmlDocument::parse(&completion_xml)?)?;

    let challenge_xml = client.request_https(
        "pair",
        &[
            ("updateState", "1".to_owned()),
            ("phrase", "pairchallenge".to_owned()),
        ],
        false,
    )?;
    require_paired(&XmlDocument::parse(&challenge_xml)?)?;
    Ok(PairingOutcome::Paired {
        certificate_der: certificate_der.to_vec(),
    })
}

fn require_paired(document: &XmlDocument) -> Result<()> {
    if document.required("paired")? == "1" {
        Ok(())
    } else {
        Err(Error::Pairing("host rejected the pairing stage".to_owned()))
    }
}

fn decode_field(document: &XmlDocument, field: &str) -> Result<Vec<u8>> {
    hex::decode(document.required(field)?)
        .map_err(|error| Error::Pairing(format!("invalid `{field}` encoding: {error}")))
}

fn aes_ecb(input: &[u8], key: &[u8; 16], mode: Mode) -> Result<Vec<u8>> {
    let cipher = Cipher::aes_128_ecb();
    let block_size = cipher.block_size();
    let rounded_length = input.len().div_ceil(block_size) * block_size;
    let mut padded = Zeroizing::new(vec![0_u8; rounded_length]);
    padded[..input.len()].copy_from_slice(input);
    let mut output = vec![0_u8; rounded_length + block_size];
    let mut crypter = Crypter::new(cipher, mode, key, None)?;
    crypter.pad(false);
    let count = crypter.update(&padded, &mut output)?;
    let final_count = crypter.finalize(&mut output[count..])?;
    output.truncate(count + final_count);
    Ok(output)
}

fn verify_signature(certificate: &X509, data: &[u8], signature: &[u8]) -> Result<()> {
    let key = certificate.public_key()?;
    let mut verifier = Verifier::new(MessageDigest::sha256(), &key)?;
    verifier.update(data)?;
    if verifier.verify(signature)? {
        Ok(())
    } else {
        Err(Error::Pairing(
            "host certificate did not authenticate the pairing secret".to_owned(),
        ))
    }
}

fn sign(key: &PKeyRef<Private>, data: &[u8]) -> Result<Vec<u8>> {
    let mut signer = Signer::new(MessageDigest::sha256(), key)?;
    signer.update(data)?;
    Ok(signer.sign_to_vec()?)
}

// These accessors keep the private identity fields encapsulated in `NvClient` while pairing
// remains a sibling protocol module.
fn client_certificate_hex(client: &NvClient) -> Result<String> {
    Ok(hex::encode_upper(client.identity_certificate_pem()?))
}

fn client_certificate_signature(client: &NvClient) -> &[u8] {
    client.identity_certificate_signature()
}

fn client_identity_key(client: &NvClient) -> &PKeyRef<Private> {
    client.identity_private_key()
}
