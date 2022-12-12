use std::{net::SocketAddr, sync::Arc, time::Duration};

use rustls::{
    cipher_suite::{
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    },
    server::{NoClientAuth, ResolvesServerCertUsingSni},
    sign::{any_supported_type, CertifiedKey},
    ServerConfig, SupportedCipherSuite, ALL_CIPHER_SUITES, ALL_KX_GROUPS, ALL_VERSIONS,
};
use tokio::net::TcpSocket;

use super::{
    certificate::{CertificateResolver, TLS12_VERSION, TLS13_VERSION},
    utils::{AsKey, ParseKey, ParseValue},
    Config, Listener, Server, ServerProtocol,
};

impl Config {
    pub fn build_servers(&self) -> super::Result<Vec<Server>> {
        let mut servers: Vec<Server> = Vec::new();

        for (internal_id, id) in self.sub_keys("server.listener").enumerate() {
            let mut server = self.build_server(id)?;
            if !servers.iter().any(|s| s.id == server.id) {
                server.internal_id = internal_id as u64;
                servers.push(server);
            } else {
                return Err(format!("Duplicate listener id {:?}.", server.id));
            }
        }

        if !servers.is_empty() {
            Ok(servers)
        } else {
            Err("No server directives found in config file.".to_string())
        }
    }

    fn build_server(&self, id: &str) -> super::Result<Server> {
        // Build TLS config
        let (tls, tls_implicit) = if self
            .property_or_default(("server.listener", id, "tls.enable"), "server.tls.enable")?
            .unwrap_or(false)
        {
            // Parse protocol versions
            let mut tls_v2 = false;
            let mut tls_v3 = false;
            for (key, protocol) in self.values_or_default(
                ("server.listener", id, "tls.protocols"),
                "server.tls.protocols",
            ) {
                match protocol {
                    "TLSv1.2" | "0x0303" => tls_v2 = true,
                    "TLSv1.3" | "0x0304" => tls_v3 = true,
                    protocol => {
                        return Err(format!(
                            "Unsupported TLS protocol {:?} found in key {:?}",
                            protocol, key
                        ))
                    }
                }
            }

            // Parse cipher suites
            let mut ciphers = Vec::new();
            for (key, protocol) in
                self.values_or_default(("server.listener", id, "tls.cipher"), "server.tls.cipher")
            {
                ciphers.push(protocol.parse_key(key)?);
            }

            // Obtain default certificate
            let cert_id = self
                .value_or_default(
                    ("server.listener", id, "tls.certificate"),
                    "server.tls.certificate",
                )
                .ok_or_else(|| format!("Undefined certificate id for listener {:?}.", id))?;
            let cert = self.rustls_certificate(cert_id)?;
            let pki = self.rustls_private_key(cert_id)?;

            // Add SNI certificates
            let mut resolver = ResolvesServerCertUsingSni::new();
            let mut has_sni = false;
            for (key, value) in
                self.values_or_default(("server.listener", id, "tls.sni"), "server.tls.sni")
            {
                if let Some(prefix) = key.strip_suffix(".subject") {
                    has_sni = true;
                    resolver
                        .add(
                            value,
                            match self.value((prefix, "certificate")) {
                                Some(sni_cert_id) if sni_cert_id != cert_id => CertifiedKey {
                                    cert: vec![self.rustls_certificate(sni_cert_id)?],
                                    key: any_supported_type(&self.rustls_private_key(sni_cert_id)?)
                                        .map_err(|err| {
                                            format!(
                                                "Failed to sign SNI certificate for {:?}: {}",
                                                key, err
                                            )
                                        })?,
                                    ocsp: None,
                                    sct_list: None,
                                },
                                _ => CertifiedKey {
                                    cert: vec![cert.clone()],
                                    key: any_supported_type(&pki).map_err(|err| {
                                        format!(
                                            "Failed to sign SNI certificate for {:?}: {}",
                                            key, err
                                        )
                                    })?,
                                    ocsp: None,
                                    sct_list: None,
                                },
                            },
                        )
                        .map_err(|err| {
                            format!("Failed to add SNI certificate for {:?}: {}", key, err)
                        })?;
                }
            }

            // Add default certificate
            let default_cert = Some(Arc::new(CertifiedKey {
                cert: vec![cert],
                key: any_supported_type(&pki).map_err(|err| {
                    format!("Failed to sign certificate id {:?}: {}", cert_id, err)
                })?,
                ocsp: None,
                sct_list: None,
            }));

            // Build server config
            let mut config = ServerConfig::builder()
                .with_cipher_suites(if !ciphers.is_empty() {
                    &ciphers
                } else {
                    ALL_CIPHER_SUITES
                })
                .with_kx_groups(&ALL_KX_GROUPS)
                .with_protocol_versions(if tls_v3 == tls_v2 {
                    ALL_VERSIONS
                } else if tls_v3 {
                    TLS13_VERSION
                } else {
                    TLS12_VERSION
                })
                .map_err(|err| format!("Failed to build TLS config: {}", err))?
                .with_client_cert_verifier(NoClientAuth::new())
                .with_cert_resolver(Arc::new(CertificateResolver {
                    resolver: if has_sni { resolver.into() } else { None },
                    default_cert,
                }));

            //config.key_log = Arc::new(KeyLogger::default());
            config.ignore_client_order = self
                .property_or_default(
                    ("server.listener", id, "tls.ignore_client_order"),
                    "server.tls.ignore_client_order",
                )?
                .unwrap_or(true);
            (
                config.into(),
                self.property_or_default(
                    ("server.listener", id, "tls.implicit"),
                    "server.tls.implicit",
                )?
                .unwrap_or(true),
            )
        } else {
            (None, false)
        };

        // Build listeners
        let mut listeners = Vec::new();
        for result in self.properties::<SocketAddr>(("server.listener", id, "bind")) {
            // Parse bind address and build socket
            let (_, addr) = result?;
            let socket = if addr.is_ipv4() {
                TcpSocket::new_v4()
            } else {
                TcpSocket::new_v6()
            }
            .map_err(|err| format!("Failed to create socket: {}", err))?;
            let mut backlog = None;
            let mut ttl = None;

            // Set socket options
            for option in [
                "reuse-addr",
                "reuse-port",
                "send-buffer-size",
                "recv-buffer-size",
                "recv-buffer-size",
                "linger",
                "tos",
                "backlog",
                "ttl",
            ] {
                if let Some(value) = self.value_or_default(
                    ("server.listener", id, "socket", option),
                    ("server.listener.socket", option),
                ) {
                    let key = ("server.listener", id, "socket", option);
                    match option {
                        "reuse-addr" => socket.set_reuseaddr(value.parse_key(key)?),
                        "reuse-port" => socket.set_reuseport(value.parse_key(key)?),
                        "send-buffer-size" => socket.set_send_buffer_size(value.parse_key(key)?),
                        "recv-buffer-size" => socket.set_recv_buffer_size(value.parse_key(key)?),
                        "linger" => {
                            socket.set_linger(Duration::from_millis(value.parse_key(key)?).into())
                        }
                        "tos" => socket.set_tos(value.parse_key(key)?),
                        "backlog" => {
                            backlog = Some(value.parse_key(key)?);
                            continue;
                        }
                        "ttl" => {
                            ttl = Some(value.parse_key(key)?);
                            continue;
                        }
                        _ => unreachable!(),
                    }
                    .map_err(|err| {
                        format!(
                            "Failed to set socket option '{}' for listener '{}': {}",
                            option, id, err
                        )
                    })?;
                }
            }

            listeners.push(Listener {
                socket,
                addr,
                ttl,
                backlog,
            });
        }

        if listeners.is_empty() {
            return Err(format!(
                "No 'bind' directive found for listener id {:?}",
                id
            ));
        }

        Ok(Server {
            id: id.to_string(),
            internal_id: 0,
            hostname: self
                .value_or_default(("server.listener", id, "hostname"), "server.hostname")
                .ok_or("Hostname directive not found.")?
                .to_string(),
            greeting: self
                .value_or_default(("server.listener", id, "greeting"), "server.greeting")
                .unwrap_or("Stalwart SMTP at your service")
                .to_string(),
            protocol: self
                .property_or_default(("server.listener", id, "protocol"), "server.protocol")?
                .unwrap_or(ServerProtocol::Smtp),
            listeners,
            tls,
            tls_implicit,
        })
    }
}

impl ParseValue for ServerProtocol {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        if value.eq_ignore_ascii_case("smtp") {
            Ok(Self::Smtp)
        } else if value.eq_ignore_ascii_case("lmtp") {
            Ok(Self::Lmtp)
        } else {
            Err(format!(
                "Invalid server protocol type {:?} for property {:?}.",
                value,
                key.as_key()
            ))
        }
    }
}

impl ParseValue for SocketAddr {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        value.parse().map_err(|_| {
            format!(
                "Invalid socket address {:?} for property {:?}.",
                value,
                key.as_key()
            )
        })
    }
}

impl ParseValue for SupportedCipherSuite {
    fn parse_value(key: impl AsKey, value: &str) -> super::Result<Self> {
        Ok(match value {
            // TLS1.3 suites
            "TLS13_AES_256_GCM_SHA384" => TLS13_AES_256_GCM_SHA384,
            "TLS13_AES_128_GCM_SHA256" => TLS13_AES_128_GCM_SHA256,
            "TLS13_CHACHA20_POLY1305_SHA256" => TLS13_CHACHA20_POLY1305_SHA256,
            // TLS1.2 suites
            "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => {
                TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
            }
            "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
            "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => {
                TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256
            }
            cipher => {
                return Err(format!(
                    "Unsupported TLS cipher suite {:?} found in key {:?}",
                    cipher,
                    key.as_key()
                ))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use crate::config::Config;

    #[test]
    fn build_servers() {
        let mut file = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        file.push("resources");
        file.push("tests");
        file.push("parser");
        file.push("servers.toml");

        let mut cert_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        cert_path.push("resources");
        cert_path.push("tests");
        cert_path.push("certs");
        let mut cert = cert_path.clone();
        cert.push("tls_cert.pem");
        let mut pk = cert_path.clone();
        pk.push("tls_privatekey.pem");

        let toml = fs::read_to_string(file)
            .unwrap()
            .replace("{CERT}", cert.as_path().to_str().unwrap())
            .replace("{PK}", pk.as_path().to_str().unwrap());

        let config = Config::parse(&toml).unwrap();
        let servers = config.build_servers().unwrap();

        println!("{:#?}", servers);
    }
}