use crate::ReadResult;
use crate::{ClientConfig, ForwardLoginInfo, TunnelType};
use anyhow::{bail, Context, Result};
use log::{debug, error, info};
use quinn::crypto::rustls::TLSError;
use quinn::TransportConfig;
use quinn::{Certificate, RecvStream, SendStream};
use rustls::ServerCertVerified;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc::Receiver;
use tokio::sync::Mutex;
use tokio::time::Duration;

const LOCAL_ADDR_STR: &str = "0.0.0.0:0";

pub struct Client {
    config: ClientConfig,
    remote_conn: Option<quinn::Connection>,
}

impl Client {
    pub fn new(config: ClientConfig) -> Self {
        Client {
            config,
            remote_conn: None,
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
        let mut transport_cfg = TransportConfig::default();
        transport_cfg
            .stream_receive_window(1024 * 1024 * 5)
            .unwrap();
        transport_cfg.receive_window(1024 * 1024 * 200).unwrap();
        transport_cfg.send_window(1024 * 1024 * 200);
        transport_cfg
            .max_idle_timeout(Some(Duration::from_millis(self.config.max_idle_timeout_ms)))
            .unwrap();
        transport_cfg.keep_alive_interval(Some(Duration::from_millis(
            self.config.keep_alive_interval_ms,
        )));

        let mut cfg = quinn::ClientConfig::default();
        cfg.transport = Arc::new(transport_cfg);

        info!("using cert: {}", self.config.cert_path);

        let cert = Client::read_cert(self.config.cert_path.as_str())?;
        let tls_cfg = Arc::get_mut(&mut cfg.crypto).unwrap();
        tls_cfg
            .dangerous()
            .set_certificate_verifier(Arc::new(CertVerifier { cert: cert.clone() }));

        let mut cfg_builder = quinn::ClientConfigBuilder::new(cfg);
        cfg_builder.add_certificate_authority(cert)?;
        //cfg_builder.protocols(&[b"\x05rstun"]);
        cfg_builder.enable_keylog();

        let remote_addr = self
            .config
            .server_addr
            .parse()
            .with_context(|| format!("invalid address: {}", self.config.server_addr))?;

        let local_addr = LOCAL_ADDR_STR.parse().unwrap();

        let mut endpoint_builder = quinn::Endpoint::builder();
        endpoint_builder.default_client_config(cfg_builder.build());

        let (endpoint, _) = endpoint_builder.bind(&local_addr)?;

        info!(
            "connecting to {}, local_addr: {}",
            remote_addr,
            endpoint.local_addr().unwrap()
        );

        let quinn::NewConnection { connection, .. } = endpoint
            .connect(&remote_addr, "localhost")?
            .await
            .context("connect failed!")?;

        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|e| error!("open bidirectional connection failed: {}", e))
            .unwrap();

        info!("logging in... server: {}", remote_addr);

        Self::send_login_info(&self.config, &mut send, &mut recv).await?;

        info!("logged in! server: {}", remote_addr);

        self.remote_conn = Some(connection);
        Ok(())
    }

    pub async fn serve(&mut self, local_conn_receiver: &mut Receiver<TcpStream>) -> Result<()> {
        info!("start serving...");

        let remote_conn = &self.remote_conn.as_ref().unwrap();
        // accept local connections and build a tunnel to remote for accepted connections
        while let Some(local_conn) = local_conn_receiver.recv().await {
            match remote_conn.open_bi().await {
                Ok((remote_send, remote_recv)) => {
                    tokio::spawn(Self::handle_stream(local_conn, remote_send, remote_recv));
                }
                Err(e) => {
                    error!("failed to open_bi on remote connection: {}", e);
                    break;
                }
            }
        }

        info!("quit!");
        Ok(())
    }

    async fn handle_stream(
        local_conn: TcpStream,
        remote_send: SendStream,
        remote_recv: RecvStream,
    ) -> Result<()> {
        let stream_id = remote_send.id().index();
        info!("open new stream, id: {}", stream_id);

        let (local_read, local_write) = tokio::io::split(local_conn);
        let local_read = Arc::new(Mutex::new(local_read));
        let local_write = Arc::new(Mutex::new(local_write));
        let remote_send = Arc::new(Mutex::new(remote_send));
        let remote_recv = Arc::new(Mutex::new(remote_recv));

        let mut local_read_result = ReadResult::Succeeded;

        loop {
            let local_read = local_read.clone();
            let local_write = local_write.clone();
            let remote_send = remote_send.clone();
            let remote_recv = remote_recv.clone();
            let h1 =
                tokio::spawn(async move { Self::local_to_remote(local_read, remote_send).await });
            let h2 =
                tokio::spawn(async move { Self::remote_to_local(remote_recv, local_write).await });

            tokio::select! {
                Ok(Ok(result)) = h1, if !local_read_result.is_eof() => {
                    local_read_result = result;
                }
                Ok(Ok(result)) = h2 => {
                    if let ReadResult::EOF = result {
                        info!("quit stream after hitting EOF, stream_id: {}", stream_id);
                        break;
                    }
                }
                else => {
                    info!("quit unexpectedly, stream_id: {}", stream_id);
                    break;
                }
            };
        }
        Ok(())
    }

    async fn local_to_remote(
        local_read: Arc<Mutex<ReadHalf<TcpStream>>>,
        remote_send: Arc<Mutex<SendStream>>,
    ) -> Result<ReadResult> {
        let mut buffer = vec![0_u8; 8192];
        let len_read = local_read.lock().await.read(&mut buffer[..]).await?;

        if len_read > 0 {
            remote_send
                .lock()
                .await
                .write_all(&buffer[..len_read])
                .await?;
            Ok(ReadResult::Succeeded)
        } else {
            Ok(ReadResult::EOF)
        }
    }

    async fn remote_to_local(
        remote_recv: Arc<Mutex<RecvStream>>,
        local_write: Arc<Mutex<WriteHalf<TcpStream>>>,
    ) -> Result<ReadResult> {
        let mut buffer = vec![0_u8; 8192];
        let result = remote_recv.lock().await.read(&mut buffer[..]).await?;
        if let Some(len_read) = result {
            local_write
                .lock()
                .await
                .write_all(&buffer[..len_read])
                .await?;
            return Ok(ReadResult::Succeeded);
        }
        Ok(ReadResult::EOF)
    }

    async fn send_login_info(
        config: &ClientConfig,
        send: &mut SendStream,
        recv: &mut RecvStream,
    ) -> Result<()> {
        let tun_type = TunnelType::Forward(ForwardLoginInfo {
            password: config.password.clone(),
            remote_downstream_name: config.remote_downstream_name.clone(),
        });

        let tun_type = bincode::serialize(&tun_type).unwrap();
        send.write_u16(tun_type.len() as u16).await?;
        send.write_all(&tun_type).await?;

        let mut resp = [0_u8; 2];
        recv.read(&mut resp)
            .await
            .context("read login response failed")?;

        if resp[0] != b'o' && resp[1] != b'k' {
            let mut err_buf = vec![0_u8; 128];
            recv.read_to_end(&mut err_buf).await?;
            bail!(
                "failed to login, err: {}{}{}",
                resp[0] as char,
                resp[1] as char,
                String::from_utf8_lossy(&err_buf)
            );
        }

        Ok(())
    }

    fn read_cert(cert_path: &str) -> Result<Certificate> {
        let cert = std::fs::read(cert_path).context("failed to read cert file")?;
        let cert = Certificate::from_pem(&cert[..]).context("failed to create Certificate")?;

        Ok(cert)
    }
}

struct CertVerifier {
    cert: Certificate,
}

impl rustls::ServerCertVerifier for CertVerifier {
    fn verify_server_cert(
        &self,
        _: &rustls::RootCertStore,
        presented_certs: &[rustls::Certificate],
        _: webpki::DNSNameRef,
        _: &[u8],
    ) -> Result<rustls::ServerCertVerified, rustls::TLSError> {
        if presented_certs.len() != 1 {
            return Err(TLSError::General(format!(
                "server sent {} certificates, expected one",
                presented_certs.len()
            )));
        }
        if presented_certs[0].0 != self.cert.as_der() {
            return Err(TLSError::General(format!(
                "server certificates doesn't match ours"
            )));
        }

        info!("certificate verified!");
        Ok(ServerCertVerified::assertion())
    }
}
