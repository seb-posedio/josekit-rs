use std::fmt::Display;
use std::borrow::Cow;
use std::ops::Deref;

use anyhow::bail;
use openssl::aes::{self, AesKey};
use openssl::derive::Deriver;
use openssl::hash::{Hasher, MessageDigest};
use openssl::pkey::{PKey, Private, Public};
use serde_json::{Map, Value};

use crate::der::{DerBuilder, DerType};
use crate::jose::{JoseError, JoseHeader};
use crate::jwe::{JweAlgorithm, JweDecrypter, JweEncrypter, JweHeader};
use crate::jwk::{Jwk, EcCurve, EcKeyPair, XCurve, XKeyPair};
use crate::util;

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
enum EcdhEsKeyType {
    Ec(EcCurve),
    X(XCurve),
}

impl EcdhEsKeyType {
    fn key_type(&self) -> &str {
        match self {
            Self::Ec(_) => "EC",
            Self::X(_) => "OKP",
        }
    }

    fn curve_name(&self) -> &str {
        match self {
            Self::Ec(val) => val.name(),
            Self::X(val) => val.name(),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum EcdhEsJweAlgorithm {
    /// Elliptic Curve Diffie-Hellman Ephemeral Static key agreement using Concat KDF
    EcdhEs,
    /// ECDH-ES using Concat KDF and CEK wrapped with "A128KW"
    EcdhEsA128Kw,
    /// ECDH-ES using Concat KDF and CEK wrapped with "A192KW"
    EcdhEsA192Kw,
    /// ECDH-ES using Concat KDF and CEK wrapped with "A256KW"
    EcdhEsA256Kw,
}

impl EcdhEsJweAlgorithm {
    pub fn encrypter_from_jwk(&self, jwk: &Jwk) -> Result<EcdhEsJweEncrypter, JoseError> {
        (|| -> anyhow::Result<EcdhEsJweEncrypter> {
            let key_type = match jwk.key_type() {
                val if val == "EC" || val == "OKP" => val,
                val => bail!("A parameter kty must be EC or OKP: {}", val),
            };
            match jwk.key_use() {
                Some(val) if val == "enc" => {}
                None => {}
                Some(val) => bail!("A parameter use must be enc: {}", val),
            }
            if !jwk.is_for_key_operation("deriveKey") {
                bail!("A parameter key_ops must contains deriveKey.");
            }
            match jwk.algorithm() {
                Some(val) if val == self.name() => {}
                None => {}
                Some(val) => bail!("A parameter alg must be {} but {}", self.name(), val),
            }
            let (public_key, key_type) = match jwk.parameter("crv") {
                Some(Value::String(val)) => match key_type {
                    "EC" => {
                        let curve = match val.as_str() {
                            "P-256" => EcCurve::P256,
                            "P-384" => EcCurve::P384,
                            "P-521" => EcCurve::P521,
                            "secp256k1" => EcCurve::Secp256K1,
                            val => bail!("EC key doesn't support the curve algorithm: {}", val),
                        };
                        let x = match jwk.parameter("x") {
                            Some(Value::String(val)) => base64::decode_config(val, base64::URL_SAFE_NO_PAD)?,
                            Some(_) => bail!("A parameter x must be a string."),
                            None => bail!("A parameter x is required."),
                        };
                        let y = match jwk.parameter("y") {
                            Some(Value::String(val)) => {
                                base64::decode_config(val, base64::URL_SAFE_NO_PAD)?
                            }
                            Some(_) => bail!("A parameter y must be a string."),
                            None => bail!("A parameter y is required."),
                        };
    
                        let mut vec = Vec::with_capacity(1 + x.len() + y.len());
                        vec.push(0x04);
                        vec.extend_from_slice(&x);
                        vec.extend_from_slice(&y);
    
                        let pkcs8 = EcKeyPair::to_pkcs8(&vec, true, curve);
                        let public_key = PKey::public_key_from_der(&pkcs8)?;

                        (public_key, EcdhEsKeyType::Ec(curve))
                    },
                    "OKP" => {
                        let curve = match val.as_str() {
                            "X25519" => XCurve::X25519,
                            "X448" => XCurve::X448,
                            val => bail!("OKP key doesn't support the curve algorithm: {}", val),
                        };
                        let x = match jwk.parameter("x") {
                            Some(Value::String(val)) => base64::decode_config(val, base64::URL_SAFE_NO_PAD)?,
                            Some(_) => bail!("A parameter x must be a string."),
                            None => bail!("A parameter x is required."),
                        };

                        let pkcs8 = XKeyPair::to_pkcs8(&x, true, curve);
                        let public_key = PKey::public_key_from_der(&pkcs8)?;

                        (public_key, EcdhEsKeyType::X(curve))
                    },
                    _ => unreachable!(),
                },
                Some(_) => bail!("A parameter crv must be a string."),
                None => bail!("A parameter crv is required."),
            };
            let key_id = jwk.key_id().map(|val| val.to_string());

            Ok(EcdhEsJweEncrypter {
                algorithm: self.clone(),
                key_type,
                public_key,
                key_id,
            })
        })()
        .map_err(|err| JoseError::InvalidKeyFormat(err))
    }

    pub fn decrypter_from_jwk(&self, jwk: &Jwk) -> Result<EcdhEsJweDecrypter, JoseError> {
        (|| -> anyhow::Result<EcdhEsJweDecrypter> {
            let key_type = match jwk.key_type() {
                val if val == "EC" || val == "OKP" => val,
                val => bail!("A parameter kty must be EC or OKP: {}", val),
            };
            match jwk.key_use() {
                Some(val) if val == "enc" => {}
                None => {}
                Some(val) => bail!("A parameter use must be enc: {}", val),
            }
            if !jwk.is_for_key_operation("deriveKey") {
                bail!("A parameter key_ops must contains deriveKey.");
            }
            match jwk.algorithm() {
                Some(val) if val == self.name() => {}
                None => {}
                Some(val) => bail!("A parameter alg must be {} but {}", self.name(), val),
            }
            let (private_key, key_type) = match jwk.parameter("crv") {
                Some(Value::String(val)) => match key_type {
                    "EC" => {
                        let curve = match val.as_str() {
                            "P-256" => EcCurve::P256,
                            "P-384" => EcCurve::P384,
                            "P-521" => EcCurve::P521,
                            "secp256k1" => EcCurve::Secp256K1,
                            val => bail!("EC key doesn't support the curve algorithm: {}", val),
                        };
                        let d = match jwk.parameter("d") {
                            Some(Value::String(val)) => base64::decode_config(val, base64::URL_SAFE_NO_PAD)?,
                            Some(_) => bail!("A parameter d must be a string."),
                            None => bail!("A parameter d is required."),
                        };
    
                        let mut builder = DerBuilder::new();
                        builder.begin(DerType::Sequence);
                        {
                            builder.append_integer_from_u8(1);
                            builder.append_octed_string_from_slice(&d);
                        }
                        builder.end();
    
                        let pkcs8 = EcKeyPair::to_pkcs8(&builder.build(), false, curve);
                        let private_key = PKey::private_key_from_der(&pkcs8)?;
                        
                        (private_key, EcdhEsKeyType::Ec(curve))
                    },
                    "OKP" => {
                        let curve = match val.as_str() {
                            "X25519" => XCurve::X25519,
                            "X448" => XCurve::X448,
                            val => bail!("OKP key doesn't support the curve algorithm: {}", val),
                        };
                        let d = match jwk.parameter("d") {
                            Some(Value::String(val)) => base64::decode_config(val, base64::URL_SAFE_NO_PAD)?,
                            Some(_) => bail!("A parameter d must be a string."),
                            None => bail!("A parameter d is required."),
                        };

                        let mut builder = DerBuilder::new();
                        builder.append_octed_string_from_slice(&d);
    
                        let pkcs8 = XKeyPair::to_pkcs8(&builder.build(), false, curve);
                        let private_key = PKey::private_key_from_der(&pkcs8)?;

                        (private_key, EcdhEsKeyType::X(curve))
                    },
                    _ => unreachable!(),
                },
                Some(_) => bail!("A parameter crv must be a string."),
                None => bail!("A parameter crv is required."),
            };
            let key_id = jwk.key_id().map(|val| val.to_string());

            Ok(EcdhEsJweDecrypter {
                algorithm: self.clone(),
                key_type,
                private_key,
                key_id,
            })
        })()
        .map_err(|err| JoseError::InvalidKeyFormat(err))
    }

    fn is_direct(&self) -> bool {
        match self {
            Self::EcdhEs => true,
            _ => false,
        }
    }
}

impl JweAlgorithm for EcdhEsJweAlgorithm {
    fn name(&self) -> &str {
        match self {
            Self::EcdhEs => "ECDH-ES",
            Self::EcdhEsA128Kw => "ECDH-ES+A128KW",
            Self::EcdhEsA192Kw => "ECDH-ES+A192KW",
            Self::EcdhEsA256Kw => "ECDH-ES+A256KW",
        }
    }

    fn box_clone(&self) -> Box<dyn JweAlgorithm> {
        Box::new(self.clone())
    }
}

impl Display for EcdhEsJweAlgorithm {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write_str(self.name())
    }
}

impl Deref for EcdhEsJweAlgorithm {
    type Target = dyn JweAlgorithm;

    fn deref(&self) -> &Self::Target {
        self
    }
}

#[derive(Debug, Clone)]
pub struct EcdhEsJweEncrypter {
    algorithm: EcdhEsJweAlgorithm,
    key_type: EcdhEsKeyType,
    public_key: PKey<Public>,
    key_id: Option<String>,
}

impl EcdhEsJweEncrypter {
    pub fn set_key_id(&mut self, key_id: Option<impl Into<String>>) {
        match key_id {
            Some(val) => {
                self.key_id = Some(val.into());
            },
            None => {
                self.key_id = None;
            }
        }
    }
}

impl JweEncrypter for EcdhEsJweEncrypter {
    fn algorithm(&self) -> &dyn JweAlgorithm {
        &self.algorithm
    }

    fn key_id(&self) -> Option<&str> {
        match &self.key_id {
            Some(val) => Some(val.as_ref()),
            None => None,
        }
    }

    fn encrypt(
        &self,
        header: &mut JweHeader,
        key_len: usize,
    ) -> Result<(Cow<[u8]>, Option<Vec<u8>>), JoseError> {
        (|| -> anyhow::Result<(Cow<[u8]>, Option<Vec<u8>>)> {
            let apu = match header.claim("apu") {
                Some(Value::String(val)) => {
                    let apu = base64::decode_config(val, base64::URL_SAFE_NO_PAD)?;
                    Some(apu)
                }
                Some(_) => bail!("The apu header claim must be string."),
                None => None,
            };
            let apv = match header.claim("apv") {
                Some(Value::String(val)) => {
                    let apv = base64::decode_config(val, base64::URL_SAFE_NO_PAD)?;
                    Some(apv)
                }
                Some(_) => bail!("The apv header claim must be string."),
                None => None,
            };

            header.set_algorithm(self.algorithm.name());

            let mut map = Map::new();
            map.insert(
                "kty".to_string(),
                Value::String(self.key_type.key_type().to_string()),
            );
            map.insert(
                "crv".to_string(),
                Value::String(self.key_type.curve_name().to_string()),
            );
            let private_key = match self.key_type {
                EcdhEsKeyType::Ec(val) => {
                    let keypair = EcKeyPair::generate(val)?;
                    let mut jwk: Map<String, Value> = keypair.to_jwk_public_key().into();

                    match jwk.remove("x") {
                        Some(val) => {
                            map.insert("x".to_string(), val);
                        },
                        None => unreachable!(),
                    }
                    match jwk.remove("y") {
                        Some(val) => {
                            map.insert("y".to_string(), val);
                        },
                        None => unreachable!(),
                    }
                    keypair.into_private_key()
                },
                EcdhEsKeyType::X(val) => {
                    let keypair = XKeyPair::generate(val)?;
                    let mut jwk: Map<String, Value> = keypair.to_jwk_public_key().into();

                    match jwk.remove("x") {
                        Some(val) => {
                            map.insert("x".to_string(), val);
                        },
                        None => unreachable!(),
                    }
                    keypair.into_private_key()
                },
            };

            header.set_claim("epk", Some(Value::Object(map)))?;

            let mut deriver = Deriver::new(&private_key)?;
            deriver.set_peer(&self.public_key)?;
            let derived_key = deriver.derive_to_vec()?;

            let enc = match header.content_encryption() {
                Some(val) => val,
                _ => unreachable!(),
            };

            // concat KDF
            let md = MessageDigest::sha256();
            let mut key = Vec::new();
            for i in 1..util::ceiling(key_len, md.size()) {
                let mut hasher = Hasher::new(md)?;
                hasher.update(&(i as u32).to_be_bytes())?;
                hasher.update(&derived_key)?;
                hasher.update(enc.as_bytes())?;
                if let Some(val) = &apu {
                    hasher.update(val.as_slice())?;
                }
                if let Some(val) = &apv {
                    hasher.update(val.as_slice())?;
                }
                hasher.update(&(key_len as u32).to_be_bytes())?;

                let digest = hasher.finish()?;
                key.extend(digest.to_vec());
            }
            if key.len() != key_len {
                key.truncate(key_len);
            }

            let encrypted_key = if self.algorithm != EcdhEsJweAlgorithm::EcdhEs {
                let aes = match AesKey::new_encrypt(&derived_key) {
                    Ok(val) => val,
                    Err(err) => bail!("{:?}", err),
                };

                let mut encrypted_key = vec![0; key_len + 8];
                let len = match aes::wrap_key(&aes, None, &mut encrypted_key, &key) {
                    Ok(val) => val,
                    Err(err) => bail!("{:?}", err),
                };
                if len < encrypted_key.len() {
                    encrypted_key.truncate(len);
                }
                Some(encrypted_key)
            } else {
                None
            };

            Ok((Cow::Owned(key), encrypted_key))
        })()
        .map_err(|err| match err.downcast::<JoseError>() {
            Ok(err) => err,
            Err(err) => JoseError::InvalidKeyFormat(err),
        })
    }

    fn box_clone(&self) -> Box<dyn JweEncrypter> {
        Box::new(self.clone())
    }
}

impl Deref for EcdhEsJweEncrypter {
    type Target = dyn JweEncrypter;

    fn deref(&self) -> &Self::Target {
        self
    }
}

#[derive(Debug, Clone)]
pub struct EcdhEsJweDecrypter {
    algorithm: EcdhEsJweAlgorithm,
    key_type: EcdhEsKeyType,
    private_key: PKey<Private>,
    key_id: Option<String>,
}

impl EcdhEsJweDecrypter {
    pub fn set_key_id(&mut self, key_id: Option<impl Into<String>>) {
        match key_id {
            Some(val) => {
                self.key_id = Some(val.into());
            },
            None => {
                self.key_id = None;
            }
        }
    }
}

impl JweDecrypter for EcdhEsJweDecrypter {
    fn algorithm(&self) -> &dyn JweAlgorithm {
        &self.algorithm
    }

    fn key_id(&self) -> Option<&str> {
        match &self.key_id {
            Some(val) => Some(val.as_ref()),
            None => None,
        }
    }

    fn decrypt(
        &self,
        header: &JweHeader,
        encrypted_key: Option<&[u8]>,
        key_len: usize,
    ) -> Result<Cow<[u8]>, JoseError> {
        (|| -> anyhow::Result<Cow<[u8]>> {
            match encrypted_key {
                Some(_) => {
                    if self.algorithm.is_direct() {
                        bail!("The encrypted_key must not exist.");
                    }
                }
                None => {
                    if !self.algorithm.is_direct() {
                        bail!("A encrypted_key is required.");
                    }
                }
            }

            let apu = match header.claim("apu") {
                Some(Value::String(val)) => {
                    let apu = base64::decode_config(val, base64::URL_SAFE_NO_PAD)?;
                    Some(apu)
                }
                Some(_) => bail!("The apu header claim must be string."),
                None => None,
            };
            let apv = match header.claim("apv") {
                Some(Value::String(val)) => {
                    let apv = base64::decode_config(val, base64::URL_SAFE_NO_PAD)?;
                    Some(apv)
                }
                Some(_) => bail!("The apv header claim must be string."),
                None => None,
            };

            let public_key = match header.claim("epk") {
                Some(Value::Object(map)) => {
                    match map.get("kty") {
                        Some(Value::String(val)) => {
                            if val != self.key_type.key_type() {
                                bail!("The kty parameter in epk header claim is invalid: {}", val);
                            }
                        }
                        Some(_) => bail!("The kty parameter in epk header claim must be a string."),
                        None => bail!("The kty parameter in epk header claim is required."),
                    }

                    match map.get("crv") {
                        Some(Value::String(val)) => {
                            if val != self.key_type.curve_name() {
                                bail!("The crv parameter in epk header claim is invalid: {}", val);
                            }
                        }
                        Some(_) => bail!("The crv parameter in epk header claim must be a string."),
                        None => bail!("The crv parameter in epk header claim is required."),
                    }

                    match &self.key_type {
                        EcdhEsKeyType::Ec(curve) => {
                            let x = match map.get("x") {
                                Some(Value::String(val)) => {
                                    base64::decode_config(val, base64::URL_SAFE_NO_PAD)?
                                }
                                Some(_) => {
                                    bail!("The x parameter in epk header claim must be a string.")
                                }
                                None => bail!("The x parameter in epk header claim is required."),
                            };
                            let y = match map.get("y") {
                                Some(Value::String(val)) => {
                                    base64::decode_config(val, base64::URL_SAFE_NO_PAD)?
                                }
                                Some(_) => {
                                    bail!("The x parameter in epk header claim must be a string.")
                                }
                                None => bail!("The x parameter in epk header claim is required."),
                            };

                            let mut vec = Vec::with_capacity(1 + x.len() + y.len());
                            vec.push(0x04);
                            vec.extend_from_slice(&x);
                            vec.extend_from_slice(&y);

                            let pkcs8 = EcKeyPair::to_pkcs8(&vec, true, *curve);
                            PKey::public_key_from_der(&pkcs8)?
                        }
                        EcdhEsKeyType::X(curve) => {
                            let x = match map.get("x") {
                                Some(Value::String(val)) => {
                                    base64::decode_config(val, base64::URL_SAFE_NO_PAD)?
                                }
                                Some(_) => {
                                    bail!("The x parameter in epk header claim must be a string.")
                                }
                                None => bail!("The x parameter in epk header claim is required."),
                            };

                            let pkcs8 = XKeyPair::to_pkcs8(&x, true, *curve);
                            PKey::public_key_from_der(&pkcs8)?
                        }
                        _ => unreachable!(),
                    }
                }
                Some(_) => bail!("The epk header claim must be object."),
                None => bail!("This algorithm must have epk header claim."),
            };

            let mut deriver = Deriver::new(&self.private_key)?;
            deriver.set_peer(&public_key)?;
            let derived_key = deriver.derive_to_vec()?;

            let enc = match header.content_encryption() {
                Some(val) => val,
                _ => unreachable!(),
            };

            // concat KDF
            let md = MessageDigest::sha256();
            let mut key = Vec::new();
            for i in 1..util::ceiling(key_len, md.size()) {
                let mut hasher = Hasher::new(md)?;
                hasher.update(&(i as u32).to_be_bytes())?;
                hasher.update(&derived_key)?;
                hasher.update(enc.as_bytes())?;
                if let Some(val) = &apu {
                    hasher.update(val.as_slice())?;
                }
                if let Some(val) = &apv {
                    hasher.update(val.as_slice())?;
                }
                hasher.update(&(key_len as u32).to_be_bytes())?;

                let digest = hasher.finish()?;
                key.extend(digest.to_vec());
            }
            if key.len() != key_len {
                key.truncate(key_len);
            }

            let key = if self.algorithm.is_direct() {
                let encrypted_key = match encrypted_key {
                    Some(val) => val,
                    None => unreachable!(),
                };

                let aes = match AesKey::new_encrypt(&derived_key) {
                    Ok(val) => val,
                    Err(err) => bail!("{:?}", err),
                };

                let mut key = vec![0; key_len + 8];
                let len = match aes::unwrap_key(&aes, None, &mut key, &encrypted_key) {
                    Ok(val) => val,
                    Err(err) => bail!("{:?}", err),
                };
                if len < key.len() {
                    key.truncate(len);
                }

                key
            } else {
                key
            };

            Ok(Cow::Owned(key))
        })()
        .map_err(|err| JoseError::InvalidJweFormat(err))
    }

    fn box_clone(&self) -> Box<dyn JweDecrypter> {
        Box::new(self.clone())
    }
}

impl Deref for EcdhEsJweDecrypter {
    type Target = dyn JweDecrypter;

    fn deref(&self) -> &Self::Target {
        self
    }
}
