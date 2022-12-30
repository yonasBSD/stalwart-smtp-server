use smtp_proto::RcptTo;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::core::{Session, SessionAddress};

impl<T: AsyncWrite + AsyncRead + Unpin> Session<T> {
    pub async fn handle_rcpt_to(&mut self, to: RcptTo<String>) -> Result<(), ()> {
        if self.data.mail_from.is_none() {
            return self.write(b"503 5.5.1 MAIL is required first.\r\n").await;
        } else if self.data.rcpt_to.len() >= self.params.rcpt_max {
            return self.write(b"451 4.5.3 Too many recipients.\r\n").await;
        }

        // Build RCPT
        let address_lcase = to.address.to_lowercase();
        let rcpt = SessionAddress {
            domain: address_lcase
                .rsplit_once('@')
                .map(|(_, d)| d)
                .unwrap_or_default()
                .to_string(),
            address_lcase,
            address: to.address,
            flags: to.flags,
        };

        // Verify address
        if let (Some(domain_lookup), Some(address_lookup)) = (
            &self.params.rcpt_lookup_domain,
            &self.params.rcpt_lookup_addresses,
        ) {
            if let Some(is_local_domain) = domain_lookup.exists(&rcpt.domain).await {
                if is_local_domain {
                    if let Some(is_local_address) = address_lookup.exists(&rcpt.address_lcase).await
                    {
                        if !is_local_address {
                            return self
                                .rcpt_error(b"550 5.1.2 Mailbox does not exist.\r\n")
                                .await;
                        }
                    } else {
                        return self
                            .write(b"451 4.4.3 Unable to verify address at this time.\r\n")
                            .await;
                    }
                } else if !self.params.rcpt_relay {
                    return self.rcpt_error(b"550 5.1.2 Relay not allowed.\r\n").await;
                }
            } else {
                return self
                    .write(b"451 4.4.3 Unable to verify address at this time.\r\n")
                    .await;
            }
        } else if !self.params.rcpt_relay {
            return self.rcpt_error(b"550 5.1.2 Relay not allowed.\r\n").await;
        }

        if !self.data.rcpt_to.contains(&rcpt) {
            self.data.rcpt_to.push(rcpt);
            if !self.is_allowed().await {
                self.data.rcpt_to.pop();
                return self
                    .write(b"451 4.4.5 Rate limit exceeded, try again later.\r\n")
                    .await;
            }
        }

        self.write(b"250 2.1.5 OK\r\n").await
    }

    async fn rcpt_error(&mut self, response: &[u8]) -> Result<(), ()> {
        tokio::time::sleep(self.params.rcpt_errors_wait).await;
        self.data.rcpt_errors += 1;
        self.write(response).await?;
        if self.data.rcpt_errors < self.params.rcpt_errors_max {
            Ok(())
        } else {
            self.write(b"421 4.3.0 Too many errors, disconnecting.\r\n")
                .await?;
            tracing::debug!(
                parent: &self.span,
                event = "disconnect",
                reason = "rcpt-errors",
                "Too many invalid RCPT commands."
            );
            Err(())
        }
    }
}
