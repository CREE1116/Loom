use super::*;
use jsonwebtoken::{EncodingKey, Header, encode};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::{PermissionsExt, symlink},
    sync::mpsc,
};

// Public, disposable test fixture. Never used for production credentials.
const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIAv/c6WRj8kXrPrZdaNGbDjAFgPSSVN7s1tUzQJX1FWp\n-----END PRIVATE KEY-----\n";
fn jwks() -> JwkSet {
    let der = base64::engine::general_purpose::STANDARD
        .decode("MCowBQYDK2VwAyEAZFCS6FBS49NIYpGRJ8JGR5nrMVbCP6acibJt59toZB4=")
        .unwrap();
    serde_json::from_value(json!({"keys":[{"kty":"OKP","crv":"Ed25519","kid":"test","alg":"EdDSA","use":"sig","x":URL_SAFE_NO_PAD.encode(&der[12..])}]})).unwrap()
}
fn signed(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some("test".into());
    encode(
        &header,
        claims,
        &EncodingKey::from_ed_pem(TEST_KEY.as_bytes()).unwrap(),
    )
    .unwrap()
}
fn id_claims() -> Value {
    json!({"iss":ISSUER,"aud":"oaiapp_test","sub":"account","nonce":"nonce","iat":now().unwrap(),"exp":now().unwrap()+3600})
}
fn access_claims() -> Value {
    json!({"iss":ISSUER,"aud":RESOURCE,"sub":"opaque-resource-subject","client_id":"oaiapp_test","scope":SCOPES,"iat":now().unwrap(),"nbf":now().unwrap(),"exp":now().unwrap()+3600})
}
fn tokens(refresh: &str) -> Value {
    json!({"token_type":"Bearer","access_token":signed(&access_claims()),"refresh_token":refresh,"expires_in":3600,"scope":SCOPES,"earliest_refresh_at":0})
}
fn credentials() -> Credentials {
    let mut c = Credentials {
        version: 1,
        issuer: ISSUER.into(),
        subject: "account".into(),
        client_id: "oaiapp_test".into(),
        ext_agent_host_id: "host".into(),
        email: None,
        id_token: signed(&id_claims()),
        access_token: String::new(),
        refresh_token: String::new(),
        scopes: String::new(),
        expires_at: 0,
        refresh_expires_at: 0,
        earliest_refresh_at: 0,
        nonce: "nonce".into(),
        signing_keys: jwks(),
        id_signing_keys: jwks(),
    };
    install_tokens(&mut c, &tokens("initial-refresh"), &jwks(), false).unwrap();
    c
}
fn root() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    (temp, root)
}
fn attempt(client: Option<&str>) -> Attempt {
    Attempt {
        state: "state".into(),
        nonce: "nonce".into(),
        verifier: "a".repeat(43),
        redirect: "http://127.0.0.1:34567/auth/callback".into(),
        client: client.map(str::to_owned),
    }
}
fn mock_token(status: u16, body: Value) -> (String, mpsc::Receiver<BTreeMap<String, String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        assert!(first.starts_with("POST /token "));
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).unwrap();
        tx.send(
            url::form_urlencoded::parse(&bytes)
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
        .unwrap();
        let body = body.to_string();
        write!(socket,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    });
    (format!("{base}/token"), rx)
}

#[test]
fn pkce_registration_and_returning_account_use_distinct_client_parameters() {
    let a = attempt(None);
    let url = Url::parse(&a.authorize("host", None).unwrap()).unwrap();
    let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "dynamic_agent_client");
    assert_eq!(params["agent_name_hint"], "Loom");
    assert_eq!(
        params["code_challenge"],
        URL_SAFE_NO_PAD.encode(Sha256::digest(a.verifier.as_bytes()))
    );
    assert_eq!(params["resource"], RESOURCE);
    let c = credentials();
    let a = attempt(Some("oaiapp_test"));
    let url = Url::parse(&a.authorize("host", Some(&c)).unwrap()).unwrap();
    let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert!(!params.contains_key("agent_name_hint"));
    assert_eq!(params["client_id"], "oaiapp_test");
    assert_eq!(params["id_token_hint"], c.id_token);
}

fn assert_uuid_v4_host(host: &str) {
    let uuid = host.strip_prefix("urn:uuid:").expect("UUID host URN");
    assert_eq!(uuid.len(), 36);
    for (index, byte) in uuid.bytes().enumerate() {
        if [8, 13, 18, 23].contains(&index) {
            assert_eq!(byte, b'-');
        } else {
            assert!(byte.is_ascii_hexdigit());
        }
    }
    assert_eq!(uuid.as_bytes()[14], b'4');
    assert!(matches!(uuid.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
}

#[test]
fn oauth_host_authorize_uses_persisted_uuid_v4_urn() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    let host = store.host().unwrap();
    assert_uuid_v4_host(&host);
    let url = Url::parse(&attempt(None).authorize(&host, None).unwrap()).unwrap();
    let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert!(params["ext_agent_host_id"] == host);
    drop(store);
    let reopened = Store::open(&root, false).unwrap();
    assert!(reopened.host().unwrap() == host);
    assert_eq!(
        fs::metadata(root.join("chatgpt-oauth/host-id.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn oauth_host_repairs_only_legacy_random_id_and_preserves_credentials() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    let legacy = format!("loom-{}", URL_SAFE_NO_PAD.encode([42u8; 32]));
    store
        .atomic("host-id.json", &serde_json::to_vec(&legacy).unwrap())
        .unwrap();
    let mut c = credentials();
    c.ext_agent_host_id = legacy;
    store.save("a", &c).unwrap();
    let before = store.read("a.json").unwrap().unwrap();
    let host = store.host().unwrap();
    assert_uuid_v4_host(&host);
    assert!(store.host().unwrap() == host);
    assert!(store.read("a.json").unwrap().unwrap() == before);
    let loaded = store.load("a").unwrap().unwrap();
    // A historical successful registration remains usable under its original
    // identity, but must never be silently rebound to the repaired host.
    assert!(loaded.renewable(now().unwrap()));
    assert!(
        attempt(Some(&loaded.client_id))
            .authorize(&host, Some(&loaded))
            .is_err()
    );
    let (endpoint, rx) = mock_token(200, tokens("legacy-rotated-refresh"));
    refresh_session(&store, "a", &mut c, &endpoint, &jwks()).unwrap();
    let form = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(form["client_id"] == "oaiapp_test");
    let refreshed = store.load("a").unwrap().unwrap();
    assert!(refreshed.ext_agent_host_id == loaded.ext_agent_host_id);
    assert!(refreshed.client_id == loaded.client_id);
    assert!(refreshed.renewable(now().unwrap()));
}

#[test]
fn oauth_host_preserves_existing_supported_identifiers() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    for host in [
        "urn:uuid:12345678-1234-4abc-8def-123456789abc".to_owned(),
        format!(
            "urn:ietf:params:oauth:jwk-thumbprint:sha-256:{}",
            URL_SAFE_NO_PAD.encode([42u8; 32])
        ),
        "did:key:z6MkrJVnaZkeFzdQyYmPyskXBzzPJQhLXbaZKYVfjc6MHkBc".to_owned(),
    ] {
        let bytes = serde_json::to_vec(&host).unwrap();
        store.atomic("host-id.json", &bytes).unwrap();
        assert!(store.host().unwrap() == host);
        assert!(store.read("host-id.json").unwrap().unwrap() == bytes);
        let mut c = credentials();
        c.ext_agent_host_id = host.clone();
        assert!(
            attempt(Some(&c.client_id))
                .authorize(&host, Some(&c))
                .is_ok()
        );
    }
}

#[test]
fn oauth_host_unknown_malformed_ids_are_not_migrated() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    for host in [
        "loom-short".to_owned(),
        format!("loom-{}!", "a".repeat(42)),
        format!("loom-{}B", "a".repeat(42)), // Noncanonical base64 trailing bits.
        "".to_owned(),
        "urn:uuid:12345678-1234-1abc-8def-123456789abc".to_owned(),
    ] {
        let bytes = serde_json::to_vec(&host).unwrap();
        store.atomic("host-id.json", &bytes).unwrap();
        assert!(store.host().is_err());
        assert!(store.read("host-id.json").unwrap().unwrap() == bytes);
    }
}

#[test]
fn oauth_host_returning_authorization_rejects_identity_mismatch() {
    let host = "urn:uuid:12345678-1234-4abc-8def-123456789abc";
    let mut c = credentials();
    c.ext_agent_host_id = "urn:uuid:12345678-1234-4abc-9def-123456789abc".into();
    let before = serde_json::to_vec(&c).unwrap();
    assert!(
        attempt(Some(&c.client_id))
            .authorize(host, Some(&c))
            .is_err()
    );
    c.ext_agent_host_id = host.into();
    assert!(
        attempt(Some("oaiapp_other"))
            .authorize(host, Some(&c))
            .is_err()
    );
    assert!(attempt(None).authorize(host, Some(&c)).is_err());
    c.ext_agent_host_id = "urn:uuid:12345678-1234-4abc-9def-123456789abc".into();
    assert!(serde_json::to_vec(&c).unwrap() == before);
}

#[test]
fn callback_rejects_state_denial_duplicate_and_client_substitution() {
    let a = attempt(None);
    assert!(
        a.callback("/auth/callback?state=state&code=code&client_id=oaiapp_test")
            .is_ok()
    );
    for target in [
        "/auth/callback?state=wrong&code=secret&client_id=oaiapp_test",
        "/auth/callback?state=state&error=access_denied&code=secret",
        "/auth/callback?state=state&state=state&code=secret&client_id=oaiapp_test",
        "/auth/callback?state=state&code=secret&client_id=dynamic_agent_client",
        "/auth/callback?state=state&code=secret",
        "/callback?state=state&code=secret",
    ] {
        let error = a.callback(target).err().unwrap();
        assert!(!format!("{error:?}").contains("secret"));
    }
    let a = attempt(Some("oaiapp_test"));
    assert!(a.callback("/auth/callback?state=state&code=code").is_ok());
    assert!(
        a.callback("/auth/callback?state=state&code=code&client_id=oaiapp_other")
            .is_err()
    );
}
#[test]
fn signed_identity_requires_nonce_issuer_audience_expiry_and_signature() {
    assert!(identity(&signed(&id_claims()), "oaiapp_test", "nonce", &jwks()).is_ok());
    for (field, value) in [
        ("nonce", json!("other")),
        ("iss", json!("https://evil.invalid")),
        ("aud", json!("oaiapp_other")),
        ("exp", json!(1)),
        ("nbf", json!(now().unwrap() + 1000)),
    ] {
        let mut claims = id_claims();
        claims[field] = value;
        assert!(identity(&signed(&claims), "oaiapp_test", "nonce", &jwks()).is_err());
    }
    let token = signed(&id_claims());
    let mut pieces: Vec<_> = token.split('.').collect();
    pieces[2] = "aW52YWxpZA";
    assert!(identity(&pieces.join("."), "oaiapp_test", "nonce", &jwks()).is_err());
    let mut key = jwks();
    key.keys.clear();
    assert!(identity(&token, "oaiapp_test", "nonce", &key).is_err());
}
#[test]
fn identity_only_scope_never_enables_inference_and_refresh_requires_rotation() {
    let mut c = credentials();
    let mut token = tokens("new-refresh");
    token["scope"] = json!("openid profile email offline_access resource.invoke");
    assert!(install_tokens(&mut c, &token, &jwks(), true).is_err());
    let token = tokens("initial-refresh");
    assert!(install_tokens(&mut c, &token, &jwks(), true).is_err());
    let mut token = tokens("new-refresh");
    let mut claims = access_claims();
    claims["scope"] = json!("openid profile email");
    token["access_token"] = json!(signed(&claims));
    assert!(install_tokens(&mut c, &token, &jwks(), true).is_err());
    let mut token = tokens("new-refresh");
    token.as_object_mut().unwrap().remove("scope");
    install_tokens(&mut c, &token, &jwks(), true).unwrap();
    assert!(c.renewable(now().unwrap()));
}
#[test]
fn refresh_updates_atomic_record_and_preserves_issued_client_and_resource() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    let _lock = store.lock("a").unwrap();
    let mut c = credentials();
    store.save("a", &c).unwrap();
    let (endpoint, rx) = mock_token(200, tokens("rotated-refresh"));
    refresh_session(&store, "a", &mut c, &endpoint, &jwks()).unwrap();
    let form = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["client_id"], "oaiapp_test");
    assert_eq!(form["resource"], RESOURCE);
    assert!(!form.contains_key("scope"));
    let loaded = store.load("a").unwrap().unwrap();
    assert_eq!(loaded.refresh_token, "rotated-refresh");
    assert!(loaded.renewable(now().unwrap()));
    assert_eq!(
        fs::metadata(root.join("chatgpt-oauth/a.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        fs::read_dir(root.join("chatgpt-oauth"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pending-"))
    );
}
#[test]
fn terminal_refresh_clears_tokens_but_transient_failure_keeps_them() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    let _lock = store.lock("a").unwrap();
    let mut c = credentials();
    store.save("a", &c).unwrap();
    let (endpoint, rx) = mock_token(
        503,
        json!({"error":"temporarily_unavailable","detail":"secret-reflected"}),
    );
    let error = refresh_session(&store, "a", &mut c, &endpoint, &jwks()).unwrap_err();
    let _ = rx.recv().unwrap();
    assert_eq!(error.kind, ErrorKind::Transient);
    assert!(!format!("{error:?}").contains("secret"));
    assert_eq!(
        store.load("a").unwrap().unwrap().refresh_token,
        "initial-refresh"
    );
    let (endpoint, rx) = mock_token(400, json!({"error":"invalid_grant"}));
    assert!(refresh_session(&store, "a", &mut c, &endpoint, &jwks()).is_err());
    let _ = rx.recv().unwrap();
    let loaded = store.load("a").unwrap().unwrap();
    assert!(
        loaded.refresh_token.is_empty()
            && loaded.access_token.is_empty()
            && loaded.id_token.is_empty()
    );
    assert_eq!(loaded.client_id, "oaiapp_test");
    assert_eq!(loaded.subject, "account");
    assert!(!is_connected(&root, "a"));
}
#[test]
fn refresh_invalid_success_never_reuses_consumed_token() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    let _lock = store.lock("a").unwrap();
    let mut c = credentials();
    store.save("a", &c).unwrap();
    let (endpoint, rx) = mock_token(
        200,
        json!({"token_type":"Bearer","access_token":"secret-invalid","refresh_token":"new"}),
    );
    assert!(refresh_session(&store, "a", &mut c, &endpoint, &jwks()).is_err());
    let _ = rx.recv().unwrap();
    assert!(store.load("a").unwrap().unwrap().refresh_token.is_empty());
}
#[test]
fn retired_identity_key_is_retained_and_unknown_refresh_key_reloads_once() {
    let mut c = credentials();
    // An early version-1 record may lack the separate retained ID-key cache.
    let mut legacy = serde_json::to_value(&c).unwrap();
    legacy.as_object_mut().unwrap().remove("id_signing_keys");
    c = serde_json::from_value(legacy).unwrap();
    assert!(c.renewable(now().unwrap()));
    let mut new_keys = jwks();
    new_keys.keys[0].common.key_id = Some("rotated".into());
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some("rotated".into());
    let access = encode(
        &header,
        &access_claims(),
        &EncodingKey::from_ed_pem(TEST_KEY.as_bytes()).unwrap(),
    )
    .unwrap();
    let mut token = tokens("new-refresh");
    token["access_token"] = json!(access);
    let mut reloads = 0;
    install_rotated(&mut c, &token, &jwks(), &mut || {
        reloads += 1;
        Ok(new_keys.clone())
    })
    .unwrap();
    assert_eq!(reloads, 1);
    assert!(c.renewable(now().unwrap()));
    assert!(c.signing_keys.find("test").is_none());
    assert!(c.id_signing_keys.find("test").is_some());
}
#[test]
fn durable_pending_rotation_recovers_before_old_credentials_can_be_used() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    let _lock = store.lock("a").unwrap();
    let mut c = credentials();
    store.save("a", &c).unwrap();
    install_tokens(&mut c, &tokens("durable-replacement"), &jwks(), true).unwrap();
    let mut pending = store.file(".a.json.pending", true).unwrap().unwrap();
    pending.write_all(&serde_json::to_vec(&c).unwrap()).unwrap();
    pending.sync_all().unwrap();
    drop(pending);
    let loaded = store.load("a").unwrap().unwrap();
    assert_eq!(loaded.refresh_token, "durable-replacement");
    assert!(store.file(".a.json.pending", false).unwrap().is_none());
    let mut pending = store.file(".a.json.pending", true).unwrap().unwrap();
    pending.write_all(b"{incomplete").unwrap();
    pending.sync_all().unwrap();
    let interrupted = store.load("a").unwrap().unwrap();
    assert!(interrupted.access_token.is_empty() && interrupted.refresh_token.is_empty());
    assert_eq!(interrupted.client_id, "oaiapp_test");
    store.save("a", &credentials()).unwrap();
    assert!(store.load("a").unwrap().unwrap().renewable(now().unwrap()));
}
#[test]
fn refresh_locks_serialize_two_callers_and_second_uses_rotated_record() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    let mut c = credentials();
    c.expires_at = now().unwrap() + 30;
    store.save("a", &c).unwrap();
    let (endpoint, rx) = mock_token(200, tokens("rotated-refresh"));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let root = root.clone();
            let endpoint = endpoint.clone();
            std::thread::spawn(move || {
                let store = Store::open(&root, false).unwrap();
                let _lock = store.lock("a").unwrap();
                let mut c = store.load("a").unwrap().unwrap();
                if c.expires_at <= now().unwrap() + REFRESH_MARGIN {
                    refresh_session(&store, "a", &mut c, &endpoint, &jwks()).unwrap();
                }
                c.refresh_token
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), "rotated-refresh");
    }
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap()["refresh_token"],
        "initial-refresh"
    );
    assert!(rx.try_recv().is_err());
}
#[test]
fn unsafe_id_public_file_symlink_hardlink_and_directory_are_rejected() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    for id in ["", "../escape", "a/b", "a.b", "host-id"] {
        assert!(store.save(id, &credentials()).is_err());
    }
    let path = root.join("chatgpt-oauth/a.json");
    store.save("a", &credentials()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load("a").is_err());
    assert!(store.save("a", &credentials()).is_err());
    assert!(!is_connected(&root, "a"));
    fs::remove_file(&path).unwrap();
    fs::write(root.join("outside"), b"do not replace").unwrap();
    symlink(root.join("outside"), &path).unwrap();
    assert!(store.load("a").is_err());
    assert!(store.save("a", &credentials()).is_err());
    assert_eq!(fs::read(root.join("outside")).unwrap(), b"do not replace");
    fs::remove_file(&path).unwrap();
    store.save("a", &credentials()).unwrap();
    fs::hard_link(&path, root.join("link")).unwrap();
    assert!(store.load("a").is_err());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(store.load("a").is_err());
    symlink(&root, root.join("alias")).unwrap();
    assert!(Store::open(&root.join("alias"), false).is_err());
    fs::set_permissions(
        root.join("chatgpt-oauth"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(Store::open(&root, false).is_err());
}
#[test]
fn logout_of_an_unconnected_account_does_not_create_state() {
    let (_temp, root) = root();
    logout(&root, "never-connected").unwrap();
    assert!(!root.join("chatgpt-oauth").exists());
}
#[test]
fn connected_requires_valid_signed_credentials_and_host_is_stable() {
    let (_temp, root) = root();
    let store = Store::open(&root, true).unwrap();
    assert_eq!(store.host().unwrap(), store.host().unwrap());
    assert!(!is_connected(&root, "a"));
    let mut c = credentials();
    store.save("a", &c).unwrap();
    assert!(is_connected(&root, "a"));
    c.access_token = "fabricated".into();
    store.save("a", &c).unwrap();
    assert!(!is_connected(&root, "a"));
    assert!(
        ChatGptProvider {
            root,
            id: "a".into()
        }
        .headers()
        .is_err()
    );
}
#[test]
fn catalog_uses_listed_slugs_in_server_order_without_fallback_models() {
    assert_eq!(catalog(&json!({"models":[{"slug":"gpt-b","visibility":"list"},{"slug":"hidden","visibility":"hidden"},{"slug":"gpt-a","visibility":"list"},{"slug":"image-only","visibility":"list"}]})).unwrap(),vec!["gpt-b","gpt-a"]);
    assert!(catalog(&json!({"data":[{"id":"gpt-fake"}]})).is_err());
}
#[test]
fn callback_read_timeout_and_browser_page_never_echo_query_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    assert!(callback_target(&mut server, Instant::now() + Duration::from_millis(20)).is_err());
    browser_result(&mut server, false);
    drop(server);
    let mut body = String::new();
    client.read_to_string(&mut body).unwrap();
    assert!(body.contains("400 Bad Request") && !body.contains("connected to ChatGPT"));
}
