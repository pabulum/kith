//! Invites: one link that makes two people friends.
//!
//! The friend gate lets a connection through only when both sides have added
//! each other, which used to mean swapping two 64-character codes in both
//! directions. An invite carries the inviter's code and name and a one-time
//! secret. Whoever pastes it adds the inviter, then shows the secret over a
//! small protocol of its own ([`ALPN`]), which the inviter's node answers
//! for someone who isn't a friend yet: by adding them back. Sharing the
//! secret is the inviter's consent, so it works once, and for a week.

use std::{
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// The pairing protocol, which strangers may speak: see [`crate::node`].
pub const ALPN: &[u8] = b"kith/pair/1";

/// How long an invite works.
pub const LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Names are cut to this many characters, in invites and from pairing.
pub const NAME_LENGTH: usize = 32;

/// The secret in an invite.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Token([u8; 16]);

impl Token {
    pub fn generate() -> Result<Self> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).context("getting randomness for an invite")?;
        Ok(Self(bytes))
    }

    /// Compares in constant time, so guessing a token over the network
    /// learns nothing from how long a refusal takes.
    pub fn matches(&self, other: &Token) -> bool {
        self.0
            .iter()
            .zip(other.0)
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl FromStr for Token {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let bytes: [u8; 16] = hex::decode(text.trim())
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .context("not an invite token")?;
        Ok(Self(bytes))
    }
}

/// What an invite link carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    /// The inviter's code.
    pub from: EndpointId,
    /// What the inviter calls themselves, as a name to save them under.
    pub name: String,
    pub token: Token,
}

/// The version byte that starts an encoded invite.
const VERSION: u8 = 1;

impl Invite {
    /// The part of `kith://invite/…` after the slash: the version, code,
    /// token and name, in lowercase base32.
    pub fn encode(&self) -> String {
        let mut bytes = vec![VERSION];
        bytes.extend_from_slice(self.from.as_bytes());
        bytes.extend_from_slice(&self.token.0);
        bytes.extend_from_slice(self.name.as_bytes());
        data_encoding::BASE32_NOPAD
            .encode(&bytes)
            .to_ascii_lowercase()
    }

    pub fn decode(text: &str) -> Result<Self> {
        let bytes = data_encoding::BASE32_NOPAD
            .decode(text.trim().to_ascii_uppercase().as_bytes())
            .ok()
            .filter(|bytes| bytes.len() >= 49)
            .context("this invite is cut short or mistyped: copy all of it")?;
        if bytes[0] != VERSION {
            bail!("this invite is from a newer Kith: update yours to use it");
        }
        let from = EndpointId::from_bytes(bytes[1..33].try_into().expect("32 bytes"))
            .ok()
            .context("this invite is mistyped: copy all of it")?;
        let token = Token(bytes[33..49].try_into().expect("16 bytes"));
        let name = clean_name(&String::from_utf8_lossy(&bytes[49..]));
        Ok(Self { from, name, token })
    }
}

/// A name someone gave themselves, made safe to show and save: no control
/// characters, none of the direction overrides that could make it read as
/// another name, trimmed, and not too long.
pub fn clean_name(name: &str) -> String {
    let bidi = |c: char| matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    name.chars()
        .filter(|&c| !c.is_control() && !bidi(c))
        .collect::<String>()
        .trim()
        .chars()
        .take(NAME_LENGTH)
        .collect::<String>()
        .trim()
        .to_string()
}

/// An invite this node made that nobody has used yet (`[[invite]]` in
/// config.toml).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outstanding {
    pub token: String,
    /// Seconds since 1970 when it stops working.
    pub expires: u64,
}

impl Outstanding {
    pub fn new(token: Token) -> Self {
        Self {
            token: token.to_string(),
            expires: now() + LIFETIME.as_secs(),
        }
    }

    pub fn expired(&self) -> bool {
        self.expires <= now()
    }

    pub fn redeems(&self, token: &Token) -> bool {
        !self.expired()
            && self
                .token
                .parse::<Token>()
                .is_ok_and(|mine| mine.matches(token))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// What someone using an invite sends the inviter's node.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub token: String,
    /// What the inviter should save them as.
    pub name: String,
}

/// The inviter's answer: their name, or why not.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reply {
    Friends { name: String },
    Refused { reason: String },
}

/// The largest request or reply read: a token and a name.
pub const MESSAGE_LIMIT: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    fn invite(name: &str) -> Invite {
        Invite {
            from: "9f79d3dea4117e8337f2780e871a3d10b2b1106ac655f97447f78b090b6c58c3"
                .parse()
                .unwrap(),
            name: name.to_string(),
            token: Token::generate().unwrap(),
        }
    }

    #[test]
    fn invites_round_trip() {
        for name in ["Sam", "", "Zoë 🎮"] {
            let original = invite(name);
            let text = original.encode();
            assert!(
                text.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            );
            assert_eq!(Invite::decode(&text).unwrap(), original);
            assert_eq!(Invite::decode(&text.to_uppercase()).unwrap(), original);
        }
        // About 80 characters with a short name, where two codes were 128.
        assert!(invite("Sam").encode().len() < 90);
    }

    #[test]
    fn broken_invites_say_what_to_do() {
        let text = invite("Sam").encode();
        let cut = Invite::decode(&text[..40]).unwrap_err().to_string();
        assert!(cut.contains("copy all of it"), "{cut}");
        let mut future = data_encoding::BASE32_NOPAD
            .decode(text.to_uppercase().as_bytes())
            .unwrap();
        future[0] = 2;
        let future = data_encoding::BASE32_NOPAD.encode(&future);
        assert!(
            Invite::decode(&future)
                .unwrap_err()
                .to_string()
                .contains("newer Kith")
        );
    }

    #[test]
    fn tokens_match_only_themselves() {
        let (a, b) = (Token::generate().unwrap(), Token::generate().unwrap());
        assert!(a.matches(&a) && !a.matches(&b));
        assert_eq!(a.to_string().parse::<Token>().unwrap(), a);
        let outstanding = Outstanding::new(a);
        assert!(outstanding.redeems(&a) && !outstanding.redeems(&b));
        let expired = Outstanding {
            expires: 1,
            ..outstanding
        };
        assert!(!expired.redeems(&a));
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Sam\n\u{7}  "), "Sam");
        assert_eq!(clean_name(&"x".repeat(100)).len(), NAME_LENGTH);
        assert_eq!(clean_name("\u{202e}live\u{202c}"), "live");
    }
}
