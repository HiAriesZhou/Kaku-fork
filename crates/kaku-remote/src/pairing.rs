//! Which browsers may open this Mac's terminals through the relay, and how a
//! new one joins.
//!
//! A browser joins by opening a pairing link that `kaku remote` prints: it
//! carries this Mac's public signing key and a one-time secret that expires
//! after `OFFER_LIFETIME`. The browser keeps its own signing key, which it
//! cannot export, and Kaku lists its public half as a paired device. Each
//! connection then runs a fresh ECDH exchange; Kaku signs the exchange first,
//! so a page talks only to the Mac it paired with, and the browser signs it
//! next, so Kaku talks only to a paired device. Removing a device from the
//! list ends its sessions within seconds.
//!
//! Every new pairing shows a notification on the Mac, so a pairing link that
//! leaked within its few minutes cannot add a device unnoticed.

use anyhow::Context;
use base64::Engine;
use ring::aead::{LessSafeKey, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{self, EcdsaKeyPair, KeyPair};
use ring::{agreement, hkdf, hmac};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Sent in the clear by Kaku as a browser connects: the tag and Kaku's
/// ephemeral P-256 public key. The page answers with `PAGE_HELLO` and its
/// own.
pub(crate) const MAC_HELLO: &[u8; 4] = b"KCH3";
pub(crate) const PAGE_HELLO: &[u8; 4] = b"KCB3";
/// An uncompressed P-256 point, as WebCrypto exports a raw public key.
pub(crate) const P256_POINT: usize = 65;
/// How long a pairing link can be used, once.
pub const OFFER_LIFETIME: Duration = Duration::from_secs(10 * 60);
const OFFER_SECRET_LEN: usize = 16;
const TRANSCRIPT_LABEL: &[u8] = b"kaku-remote v3";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Mac signing key ───────────────────────────────────────────────────────────

/// A new PKCS#8 ECDSA P-256 key for this Mac, base64url.
pub(crate) fn generate_mac_key() -> anyhow::Result<String> {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
        .map_err(|_| anyhow::anyhow!("generate signing key"))?;
    Ok(b64(pkcs8.as_ref()))
}

pub(crate) struct MacKey {
    pair: EcdsaKeyPair,
    rng: SystemRandom,
}

impl MacKey {
    pub(crate) fn from_text(text: &str) -> anyhow::Result<Self> {
        let pkcs8 = unb64(text).context("decode signing key")?;
        let rng = SystemRandom::new();
        let pair =
            EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng)
                .map_err(|_| anyhow::anyhow!("signing key is not a P-256 key"))?;
        Ok(Self { pair, rng })
    }

    pub(crate) fn public_key(&self) -> &[u8] {
        self.pair.public_key().as_ref()
    }

    /// A raw r||s signature, the form WebCrypto's ECDSA verifies.
    pub(crate) fn sign(&self, transcript: &[u8]) -> Option<Vec<u8>> {
        let sig = self.pair.sign(&self.rng, transcript).ok()?;
        Some(sig.as_ref().to_vec())
    }
}

pub(crate) fn verify(public_key: &[u8], transcript: &[u8], sig: &[u8]) -> bool {
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, public_key)
        .verify(transcript, sig)
        .is_ok()
}

// ── Per-connection exchange ───────────────────────────────────────────────────

/// What both sides sign and derive from: the Mac's relay id and both
/// ephemeral keys, so neither signature fits another connection or Mac.
pub(crate) fn transcript(host_id: &str, mac_eph: &[u8], page_eph: &[u8]) -> [u8; 32] {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(TRANSCRIPT_LABEL);
    ctx.update(host_id.as_bytes());
    ctx.update(mac_eph);
    ctx.update(page_eph);
    let digest = ctx.finish();
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// One AES-256-GCM key per direction.
pub(crate) struct SessionKeys {
    pub(crate) to_host: LessSafeKey,
    pub(crate) from_host: LessSafeKey,
}

/// HKDF-SHA256 over the ECDH secret, salted with the transcript; the same
/// derivation as the page's `crypto.subtle.deriveBits({name: "HKDF"})`.
pub(crate) fn derive_keys(shared: &[u8], transcript: &[u8; 32]) -> Option<SessionKeys> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, transcript).extract(shared);
    let key = |info: &[u8]| -> Option<LessSafeKey> {
        let info = [info];
        let okm = prk.expand(&info, &AES_256_GCM).ok()?;
        Some(LessSafeKey::new(UnboundKey::from(okm)))
    };
    Some(SessionKeys {
        to_host: key(b"kaku c2h")?,
        from_host: key(b"kaku h2c")?,
    })
}

/// Kaku's side of one connection's exchange.
pub(crate) struct Exchange {
    private: agreement::EphemeralPrivateKey,
    public: Vec<u8>,
}

impl Exchange {
    pub(crate) fn new(rng: &SystemRandom) -> Option<Self> {
        let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, rng).ok()?;
        let public = private.compute_public_key().ok()?.as_ref().to_vec();
        Some(Self { private, public })
    }

    /// `MAC_HELLO` followed by this side's public key.
    pub(crate) fn hello(&self) -> Vec<u8> {
        let mut out = MAC_HELLO.to_vec();
        out.extend_from_slice(&self.public);
        out
    }

    /// Finish with the page's hello; returns the transcript and the keys.
    pub(crate) fn finish(
        self,
        host_id: &str,
        page_hello: &[u8],
    ) -> Option<([u8; 32], SessionKeys)> {
        let page_eph = page_hello.strip_prefix(PAGE_HELLO.as_slice())?;
        if page_eph.len() != P256_POINT {
            return None;
        }
        let transcript = transcript(host_id, &self.public, page_eph);
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, page_eph);
        let keys = agreement::agree_ephemeral(self.private, &peer, |shared| {
            derive_keys(shared, &transcript)
        })
        .ok()??;
        Some((transcript, keys))
    }
}

// ── What the page proves ──────────────────────────────────────────────────────

/// The page's first sealed message: a paired device signing in, or a new one
/// joining with a pairing link.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum PageAuth {
    Auth {
        device: String,
        sig: String,
    },
    Pair {
        device: String,
        sig: String,
        proof: String,
        #[serde(default)]
        name: String,
    },
}

/// Why a page was turned away; each has its own close code so the page can
/// say what happened.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Not a paired device (removed, or the Mac was reset): 4403.
    UnknownDevice,
    /// The pairing link expired, was used, or is not this Mac's: 4401.
    PairingLink,
    /// Malformed, or a signature that does not match: 4403.
    BadProof,
}

impl Refusal {
    pub(crate) fn close_code(&self) -> u16 {
        match self {
            Refusal::PairingLink => 4401,
            Refusal::UnknownDevice | Refusal::BadProof => 4403,
        }
    }
}

/// Check the page's proof against the paired devices and the pairing offer.
/// Returns the device it is, and whether it joined just now.
pub(crate) fn admit(
    auth: &PageAuth,
    transcript: &[u8; 32],
    store: &mut dyn DeviceStore,
) -> Result<(Device, bool), Refusal> {
    let (device, sig) = match auth {
        PageAuth::Auth { device, sig } | PageAuth::Pair { device, sig, .. } => (device, sig),
    };
    let public = unb64(device).ok_or(Refusal::BadProof)?;
    let sig = unb64(sig).ok_or(Refusal::BadProof)?;
    if public.len() != P256_POINT || !verify(&public, transcript, &sig) {
        return Err(Refusal::BadProof);
    }
    let id = device_id(&public);
    let key = b64(&public);
    match auth {
        PageAuth::Auth { .. } => store
            .seen(&id, &key)
            .map(|d| (d, false))
            .ok_or(Refusal::UnknownDevice),
        PageAuth::Pair { proof, name, .. } => {
            if let Some(known) = store.seen(&id, &key) {
                return Ok((known, false));
            }
            let proof = unb64(proof).ok_or(Refusal::BadProof)?;
            let proof_matches = |secret: &[u8]| {
                let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
                hmac::verify(&key, transcript, &proof).is_ok()
            };
            if !store.take_offer(&proof_matches) {
                return Err(Refusal::PairingLink);
            }
            let device = Device {
                id,
                public_key: key,
                name: clean_name(name),
                paired_at: now(),
                last_seen: now(),
            };
            store.add(device.clone());
            Ok((device, true))
        }
    }
}

/// A short id for a device: the first 8 hex digits of its key's SHA-256.
pub(crate) fn device_id(public: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, public);
    digest.as_ref()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn clean_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(40).collect();
    let name = name.trim();
    if name.is_empty() {
        "Browser".to_string()
    } else {
        name.to_string()
    }
}

// ── Devices and the pairing offer on disk ─────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub id: String,
    pub public_key: String,
    pub name: String,
    pub paired_at: u64,
    #[serde(default)]
    pub last_seen: u64,
}

#[derive(Serialize, Deserialize)]
struct Offer {
    secret: String,
    expires_at: u64,
}

/// Where `admit` reads and records devices and the offer; files in the data
/// dir for Kaku, memory in tests.
pub(crate) trait DeviceStore {
    /// The paired device with this id and this whole public key as it was
    /// (`last_seen` is the previous visit), marking it seen now. The short id
    /// alone is not enough: it is shown to people and a key matching its 32
    /// bits can be ground offline.
    fn seen(&mut self, id: &str, public_key: &str) -> Option<Device>;
    fn add(&mut self, device: Device);
    /// Use up the current offer if `check` accepts its secret.
    fn take_offer(&mut self, check: &dyn Fn(&[u8]) -> bool) -> bool;
    /// The current offer's secret, left in place, if one is out and fresh.
    fn offer_secret(&mut self) -> Option<Vec<u8>>;
}

fn devices_path() -> PathBuf {
    config::DATA_DIR.join("remote-devices.json")
}

fn offer_path() -> PathBuf {
    config::DATA_DIR.join("remote-pairing.json")
}

/// Every device paired with this Mac.
pub fn list_devices() -> Vec<Device> {
    std::fs::read_to_string(devices_path())
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

/// Read, change and write the devices file at `path` under an exclusive
/// lock. The GUI updates it on every sign-in while `kaku remote revoke` edits
/// it from another process; without the lock the GUI could write back a list
/// read before the revoke and bring the device back. `change` returns whether
/// to save.
fn update_devices_at<T>(
    path: &std::path::Path,
    change: impl FnOnce(&mut Vec<Device>) -> (bool, T),
) -> anyhow::Result<T> {
    use std::os::unix::io::AsRawFd;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create data dir")?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))
        .context("open devices lock")?;
    // SAFETY: flock on a file descriptor we own; released when `lock` closes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        anyhow::bail!("lock paired devices: {}", std::io::Error::last_os_error());
    }
    let mut devices: Vec<Device> = std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default();
    let (save, out) = change(&mut devices);
    if save {
        let json = serde_json::to_string_pretty(&devices)?;
        crate::write_private(path, json.as_bytes()).context("save paired devices")?;
    }
    drop(lock);
    Ok(out)
}

fn update_devices<T>(change: impl FnOnce(&mut Vec<Device>) -> (bool, T)) -> anyhow::Result<T> {
    update_devices_at(&devices_path(), change)
}

fn revoke_at(path: &std::path::Path, id: &str) -> anyhow::Result<bool> {
    update_devices_at(path, |devices| {
        let before = devices.len();
        devices.retain(|d| d.id != id);
        let removed = devices.len() != before;
        (removed, removed)
    })
}

/// Remove a paired device; its open sessions end within seconds. Returns
/// whether it was paired.
pub fn revoke_device(id: &str) -> anyhow::Result<bool> {
    revoke_at(&devices_path(), id)
}

/// Forget every device and any pending pairing link.
pub fn forget_all_devices() -> anyhow::Result<()> {
    update_devices(|devices| {
        devices.clear();
        (true, ())
    })?;
    let _ = std::fs::remove_file(offer_path());
    Ok(())
}

/// Make a new pairing link's secret, replacing any earlier one. Returns it
/// base64url for the link's fragment.
pub(crate) fn new_offer() -> anyhow::Result<String> {
    let mut secret = [0u8; OFFER_SECRET_LEN];
    SystemRandom::new()
        .fill(&mut secret)
        .map_err(|_| anyhow::anyhow!("random pairing secret"))?;
    let offer = Offer {
        secret: b64(&secret),
        expires_at: now() + OFFER_LIFETIME.as_secs(),
    };
    let path = offer_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create data dir")?;
    }
    crate::write_private(&path, serde_json::to_string(&offer)?.as_bytes())
        .context("save pairing link")?;
    Ok(offer.secret)
}

fn seen_at(path: &std::path::Path, id: &str, public_key: &str) -> Option<Device> {
    update_devices_at(path, |devices| {
        match devices
            .iter_mut()
            .find(|d| d.id == id && d.public_key == public_key)
        {
            Some(device) => {
                let found = device.clone();
                device.last_seen = now();
                (true, Some(found))
            }
            None => (false, None),
        }
    })
    .ok()
    .flatten()
}

/// The devices file and offer file in Kaku's data dir.
pub(crate) struct DiskStore;

impl DeviceStore for DiskStore {
    fn seen(&mut self, id: &str, public_key: &str) -> Option<Device> {
        seen_at(&devices_path(), id, public_key)
    }

    fn add(&mut self, device: Device) {
        let added = update_devices(|devices| {
            devices.retain(|d| d.id != device.id);
            devices.push(device);
            (true, ())
        });
        if let Err(e) = added {
            log::warn!("kaku-share: {e:#}");
        }
    }

    fn take_offer(&mut self, check: &dyn Fn(&[u8]) -> bool) -> bool {
        let Some(secret) = self.offer_secret() else {
            return false;
        };
        if !check(&secret) {
            return false;
        }
        // One device per link.
        let _ = std::fs::remove_file(offer_path());
        true
    }

    fn offer_secret(&mut self) -> Option<Vec<u8>> {
        let offer = std::fs::read_to_string(offer_path())
            .ok()
            .and_then(|data| serde_json::from_str::<Offer>(&data).ok())?;
        if offer.expires_at < now() {
            let _ = std::fs::remove_file(offer_path());
            return None;
        }
        unb64(&offer.secret)
    }
}

/// Ids of the devices paired right now, for ending removed ones' sessions.
pub(crate) fn paired_ids() -> std::collections::HashSet<String> {
    list_devices().into_iter().map(|d| d.id).collect()
}

/// Tell the person at the Mac that a device joined.
pub(crate) fn announce(device: &Device) {
    let title = format!("{} can now use this Mac's terminals", device.name);
    let message = format!(
        "Paired through a Kaku remote link. If this was not you, run: kaku remote revoke {}",
        device.id
    );
    promise::spawn::spawn_into_main_thread(async move {
        wezterm_toast_notification::persistent_toast_notification(&title, &message);
    })
    .detach();
}

/// A paired device away this long says so on the Mac when it connects
/// again; reloads and reconnects within a session stay quiet.
const QUIET_RETURN: u64 = 30 * 60;

/// Whether a paired device that last connected at `last_seen` is back
/// after long enough to tell the person at the Mac.
pub(crate) fn back_after_a_while(last_seen: u64, now: u64) -> bool {
    now.saturating_sub(last_seen) >= QUIET_RETURN
}

/// Tell the person at the Mac that a paired device is using it again.
pub(crate) fn announce_return(device: &Device) {
    if !back_after_a_while(device.last_seen, now()) {
        return;
    }
    let title = format!("{} is using this Mac's terminals", device.name);
    let message = format!(
        "Connected through Kaku remote. To remove it, run: kaku remote revoke {}",
        device.id
    );
    promise::spawn::spawn_into_main_thread(async move {
        wezterm_toast_notification::persistent_toast_notification(&title, &message);
    })
    .detach();
}

/// The page link's fragment that pairs one more browser, valid for
/// `OFFER_LIFETIME`: the one-time secret alone. The Mac's key is not in it;
/// Kaku sends the key in its handshake with `mac_key_tag`, which only a
/// holder of this secret can check, and the relay never sees the fragment.
pub(crate) fn pairing_fragment(secret: &str) -> String {
    format!("#{secret}")
}

/// Kept apart from the page's own pairing proof, an HMAC of the transcript
/// alone under the same secret.
const MAC_KEY_LABEL: &[u8] = b"kaku-remote mac key";

/// Proves to a page holding the pairing link that `public` is this Mac's
/// key: an HMAC under the link's secret of the label, this connection's
/// transcript and the key, so a relay without the secret cannot vouch for
/// its own key, and a tag from another connection does not match.
pub(crate) fn mac_key_tag(secret: &[u8], transcript: &[u8], public: &[u8]) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut ctx = hmac::Context::with_key(&key);
    ctx.update(MAC_KEY_LABEL);
    ctx.update(transcript);
    ctx.update(public);
    ctx.sign().as_ref().to_vec()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    pub(crate) struct MemoryStore {
        pub(crate) devices: HashMap<String, Device>,
        pub(crate) offer: Option<(Vec<u8>, bool)>,
    }

    impl DeviceStore for MemoryStore {
        fn seen(&mut self, id: &str, public_key: &str) -> Option<Device> {
            self.devices
                .get(id)
                .filter(|d| d.public_key == public_key)
                .cloned()
        }
        fn add(&mut self, device: Device) {
            self.devices.insert(device.id.clone(), device);
        }
        fn take_offer(&mut self, check: &dyn Fn(&[u8]) -> bool) -> bool {
            match &mut self.offer {
                Some((secret, used)) if !*used && check(secret) => {
                    *used = true;
                    true
                }
                _ => false,
            }
        }
        fn offer_secret(&mut self) -> Option<Vec<u8>> {
            match &self.offer {
                Some((secret, false)) => Some(secret.clone()),
                _ => None,
            }
        }
    }

    /// A browser for tests: its own exchange and signing key, done with ring
    /// the way the page does it with WebCrypto.
    pub(crate) struct TestPage {
        exchange: Exchange,
        key: MacKey,
    }

    impl TestPage {
        pub(crate) fn new() -> Self {
            let rng = SystemRandom::new();
            Self {
                exchange: Exchange::new(&rng).unwrap(),
                key: MacKey::from_text(&generate_mac_key().unwrap()).unwrap(),
            }
        }

        pub(crate) fn hello(&self) -> Vec<u8> {
            let mut out = PAGE_HELLO.to_vec();
            out.extend_from_slice(&self.exchange.public);
            out
        }

        /// Finish against Kaku's hello; the page's keys are the Mac's with
        /// directions swapped, so return them as the Mac names them.
        pub(crate) fn finish(
            self,
            host_id: &str,
            mac_hello: &[u8],
        ) -> ([u8; 32], SessionKeys, MacKey) {
            let mac_eph = mac_hello.strip_prefix(MAC_HELLO.as_slice()).unwrap();
            let transcript = transcript(host_id, mac_eph, &self.exchange.public);
            let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, mac_eph);
            let keys = agreement::agree_ephemeral(self.exchange.private, &peer, |shared| {
                derive_keys(shared, &transcript)
            })
            .unwrap()
            .unwrap();
            (transcript, keys, self.key)
        }
    }

    fn auth(key: &MacKey, transcript: &[u8; 32]) -> PageAuth {
        PageAuth::Auth {
            device: b64(key.public_key()),
            sig: b64(&key.sign(transcript).unwrap()),
        }
    }

    fn pair(key: &MacKey, transcript: &[u8; 32], secret: &[u8]) -> PageAuth {
        let proof = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), transcript);
        PageAuth::Pair {
            device: b64(key.public_key()),
            sig: b64(&key.sign(transcript).unwrap()),
            proof: b64(proof.as_ref()),
            name: "Chrome on Windows".into(),
        }
    }

    fn exchange(host: &str) -> ([u8; 32], [u8; 32], MacKey) {
        let rng = SystemRandom::new();
        let mac = Exchange::new(&rng).unwrap();
        let page = TestPage::new();
        let page_hello = page.hello();
        let mac_hello = mac.hello();
        let (mac_t, _) = mac.finish(host, &page_hello).unwrap();
        let (page_t, _, key) = page.finish(host, &mac_hello);
        (mac_t, page_t, key)
    }

    #[test]
    fn both_sides_derive_the_same_transcript_and_keys() {
        let rng = SystemRandom::new();
        let mac = Exchange::new(&rng).unwrap();
        let page = TestPage::new();
        let (page_hello, mac_hello) = (page.hello(), mac.hello());
        let (mac_t, mac_keys) = mac.finish("host", &page_hello).unwrap();
        let (page_t, page_keys, _) = page.finish("host", &mac_hello);
        assert_eq!(mac_t, page_t);
        // What the page seals toward the Mac opens with the Mac's key.
        let nonce = [0u8; 12];
        let mut msg = b"ls\r".to_vec();
        page_keys
            .to_host
            .seal_in_place_append_tag(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                ring::aead::Aad::empty(),
                &mut msg,
            )
            .unwrap();
        let plain = mac_keys
            .to_host
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                ring::aead::Aad::empty(),
                &mut msg,
            )
            .unwrap();
        assert_eq!(plain, b"ls\r");
    }

    #[test]
    fn a_malformed_page_hello_is_refused() {
        let rng = SystemRandom::new();
        assert!(Exchange::new(&rng)
            .unwrap()
            .finish("h", b"KCB2aaaa")
            .is_none());
        let mut short = PAGE_HELLO.to_vec();
        short.extend_from_slice(&[4u8; 10]);
        assert!(Exchange::new(&rng).unwrap().finish("h", &short).is_none());
    }

    #[test]
    fn a_paired_device_signs_in_and_an_unknown_one_does_not() {
        let (transcript, _, key) = exchange("host");
        let mut store = MemoryStore::default();
        assert_eq!(
            admit(&auth(&key, &transcript), &transcript, &mut store),
            Err(Refusal::UnknownDevice)
        );
        let id = device_id(key.public_key());
        store.add(Device {
            id: id.clone(),
            public_key: b64(key.public_key()),
            name: "Safari on iPhone".into(),
            paired_at: 0,
            last_seen: 0,
        });
        assert_eq!(
            admit(&auth(&key, &transcript), &transcript, &mut store).map(|(d, _)| d.id),
            Ok(id)
        );
    }

    /// The short id is shown in notifications and `kaku remote devices`, and
    /// a key whose SHA-256 starts with the same 32 bits can be ground
    /// offline. Signing in must take the paired key itself.
    #[test]
    fn a_key_that_only_shares_the_short_id_does_not_sign_in_or_pair_over_it() {
        let (transcript, _, key) = exchange("host");
        let paired = TestPage::new()
            .finish(
                "host",
                &Exchange::new(&SystemRandom::new()).unwrap().hello(),
            )
            .2;
        let mut store = MemoryStore::default();
        // A paired device whose id the attacker's key happens to share.
        store.add(Device {
            id: device_id(key.public_key()),
            public_key: b64(paired.public_key()),
            name: "Safari on iPhone".into(),
            paired_at: 0,
            last_seen: 0,
        });
        assert_eq!(
            admit(&auth(&key, &transcript), &transcript, &mut store).map(|(d, _)| d.id),
            Err(Refusal::UnknownDevice)
        );
        // Without a pairing link it cannot pair either, and the paired device
        // is left as it was.
        assert_eq!(
            admit(
                &pair(&key, &transcript, b"no offer"),
                &transcript,
                &mut store
            )
            .map(|(d, _)| d.id),
            Err(Refusal::PairingLink)
        );
        assert_eq!(
            store.devices[&device_id(key.public_key())].public_key,
            b64(paired.public_key())
        );
    }

    /// A sign-in that read the list before a revoke must not write the
    /// device back after it.
    #[test]
    fn a_revoked_device_stays_revoked_while_it_keeps_signing_in() {
        let dir = std::env::temp_dir().join(format!("kaku-devices-{}", std::process::id()));
        let path = dir.join("remote-devices.json");
        let device = Device {
            id: "abcd1234".into(),
            public_key: "key".into(),
            name: "Chrome".into(),
            paired_at: 0,
            last_seen: 0,
        };
        for _ in 0..200 {
            update_devices_at(&path, |devices| {
                devices.clear();
                devices.push(device.clone());
                (true, ())
            })
            .unwrap();
            let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let toucher = {
                let (path, busy) = (path.clone(), busy.clone());
                std::thread::spawn(move || {
                    while busy.load(std::sync::atomic::Ordering::SeqCst) {
                        let _ = seen_at(&path, "abcd1234", "key");
                    }
                })
            };
            assert!(revoke_at(&path, "abcd1234").unwrap());
            busy.store(false, std::sync::atomic::Ordering::SeqCst);
            toucher.join().unwrap();
            assert!(
                seen_at(&path, "abcd1234", "key").is_none(),
                "the revoked device came back"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_signature_for_another_connection_does_not_sign_in() {
        // A relay that runs its own exchange with each side gets different
        // transcripts, so it cannot pass the page's signature on.
        let (mac_t, _, _) = exchange("host");
        let (other_t, _, key) = exchange("host");
        assert_ne!(mac_t, other_t);
        let mut store = MemoryStore::default();
        store.add(Device {
            id: device_id(key.public_key()),
            public_key: b64(key.public_key()),
            name: String::new(),
            paired_at: 0,
            last_seen: 0,
        });
        assert_eq!(
            admit(&auth(&key, &other_t), &mac_t, &mut store),
            Err(Refusal::BadProof)
        );
    }

    #[test]
    fn a_pairing_link_adds_one_device_once() {
        let secret = vec![7u8; OFFER_SECRET_LEN];
        let mut store = MemoryStore {
            offer: Some((secret.clone(), false)),
            ..Default::default()
        };
        let (t1, _, first) = exchange("host");
        let (added, new) = admit(&pair(&first, &t1, &secret), &t1, &mut store).unwrap();
        assert!(new);
        assert_eq!(added.name, "Chrome on Windows");
        assert!(store.devices.contains_key(&added.id));
        // The same link a second time, from another browser.
        let (t2, _, second) = exchange("host");
        assert_eq!(
            admit(&pair(&second, &t2, &secret), &t2, &mut store),
            Err(Refusal::PairingLink)
        );
        // The paired browser reconnecting with its link still open is fine.
        let (t3, _, _) = exchange("host");
        assert_eq!(
            admit(&pair(&first, &t3, &secret), &t3, &mut store),
            Ok((added, false))
        );
    }

    #[test]
    fn a_wrong_pairing_secret_adds_nothing() {
        let mut store = MemoryStore {
            offer: Some((vec![7u8; OFFER_SECRET_LEN], false)),
            ..Default::default()
        };
        let (t, _, key) = exchange("host");
        assert_eq!(
            admit(&pair(&key, &t, &[8u8; OFFER_SECRET_LEN]), &t, &mut store),
            Err(Refusal::PairingLink)
        );
        assert!(store.devices.is_empty());
        // The real link still works afterwards.
        assert!(admit(&pair(&key, &t, &[7u8; OFFER_SECRET_LEN]), &t, &mut store).is_ok());
    }

    #[test]
    fn only_a_device_back_after_a_while_is_announced() {
        assert!(back_after_a_while(0, QUIET_RETURN));
        assert!(back_after_a_while(1_000, 1_000 + QUIET_RETURN));
        // A reload or reconnect within the half hour stays quiet.
        assert!(!back_after_a_while(1_000, 1_000 + QUIET_RETURN - 1));
        assert!(!back_after_a_while(1_000, 900));
    }

    #[test]
    fn device_names_are_bounded_and_printable() {
        assert_eq!(clean_name("  "), "Browser");
        assert_eq!(clean_name("Chrome\u{7}\non Windows"), "Chromeon Windows");
        assert_eq!(clean_name(&"x".repeat(100)).len(), 40);
    }
}
