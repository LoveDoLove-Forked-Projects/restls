use anyhow::{anyhow, Context, Result};
use futures_util::stream::StreamExt;
use hmac::Mac;
use rand::Rng;
use std::{io::Cursor, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    select,
};
use tokio_util::codec::Decoder;
use tracing::debug;

use crate::{
    args::Opt,
    client_hello::ClientHello,
    client_key_exchange::ClientKeyExchange,
    common::{
        BUF_SIZE, RECORD_ALERT, RECORD_APPLICATION_DATA, RECORD_CCS, RECORD_HANDSHAKE,
        RESTLS_APPDATA_HMAC_LEN, RESTLS_APPDATA_LEN_OFFSET, RESTLS_APPDATA_OFFSET,
        RESTLS_HANDSHAKE_HMAC_LEN, TO_CLIENT_MAGIC, TO_SERVER_MAGIC,
    },
    server_hello::ServerHello,
    utils::{copy_bidirectional_fallback, tcp_rst, xor_bytes, HmacSha1, TLSCodec, TLSStream},
};

#[derive(Debug)]
enum TLS12Flow {
    Initial,
    CKEVerified,
    FullHandshakeClientCCS,
    ResumeClientCCS,
    FullHandshakeServerCCS,
    ResumeServerCCS,
    Client0x17,
}

impl TLS12Flow {
    fn ccs_from_client(&mut self) -> Result<()> {
        match self {
            TLS12Flow::CKEVerified => {
                *self = TLS12Flow::FullHandshakeClientCCS;
                Ok(())
            }
            TLS12Flow::ResumeServerCCS => {
                *self = TLS12Flow::ResumeClientCCS;
                Ok(())
            }
            _ => Err(anyhow!(
                "reject: invalid flow, expect CKEVerified or ResumeServerCCS, actual: {:?}",
                self
            )),
        }
    }

    fn ccs_from_server(&mut self) -> Result<()> {
        match self {
            TLS12Flow::Initial => {
                *self = TLS12Flow::ResumeServerCCS;
                Ok(())
            }
            TLS12Flow::FullHandshakeClientCCS => {
                *self = TLS12Flow::FullHandshakeServerCCS;
                Ok(())
            }
            _ => Err(anyhow!(
                "reject: invalid flow, expect Initial or FullHandshakeClientCCS, actual: {:?}",
                self
            )),
        }
    }

    fn cke_verified(&mut self) -> Result<()> {
        match self {
            TLS12Flow::Initial => {
                *self = TLS12Flow::CKEVerified;
                Ok(())
            }
            _ => Err(anyhow!(
                "reject: invalid flow, expect Initial, actual: {:?}",
                self
            )),
        }
    }

    fn client_0x17(&mut self) -> Result<()> {
        use TLS12Flow::*;
        match self {
            FullHandshakeServerCCS | ResumeClientCCS => {
                *self = TLS12Flow::Client0x17;
                Ok(())
            }
            _ => Err(anyhow!(
                "reject: invalid flow, expect FullHandshakeServerCCS | ResumeClientCCS, actual: {:?}",
                self
            )),
        }
    }

    fn is_ccs_from_server(&self) -> bool {
        match self {
            TLS12Flow::FullHandshakeServerCCS | TLS12Flow::ResumeServerCCS => true,
            _ => false,
        }
    }

    fn is_resume_ccs_from_client(&self) -> bool {
        match self {
            TLS12Flow::ResumeClientCCS => true,
            _ => false,
        }
    }

    fn is_resume(&self) -> bool {
        use TLS12Flow::*;

        match self {
            ResumeClientCCS | ResumeServerCCS => true,
            _ => false,
        }
    }

    fn expect_cke(&self) -> bool {
        match self {
            TLS12Flow::Initial => true,
            _ => false,
        }
    }

    fn is_client_0x17(&self) -> bool {
        match self {
            TLS12Flow::Client0x17 => true,
            _ => false,
        }
    }
}

pub struct RestlsState<'a> {
    client_hello: Option<ClientHello>,
    server_hello: Option<ServerHello>,
    client_finished: Vec<u8>,
    restls_password: &'a [u8],
    to_client_counter: u32,
    to_server_counter: u32,
    to_client_raw_counter: u32,
    mtu: usize,
    min_record_len: usize,
    id: usize,
}

fn sample_slice(data: &[u8]) -> &[u8] {
    &data[..std::cmp::min(32, data.len())]
}

impl<'a> RestlsState<'a> {
    fn restls_hmac(&self) -> HmacSha1 {
        HmacSha1::new_from_slice(self.restls_password).expect("sha1 should take key of any size")
    }

    pub fn restls_appdata_auth_hmac(&self, is_to_client: bool) -> HmacSha1 {
        let mut hasher = self.restls_hmac();
        hasher.update(&self.server_hello.as_ref().unwrap().server_random);
        if is_to_client {
            hasher.update(TO_CLIENT_MAGIC);
            hasher.update(&self.to_client_counter.to_be_bytes());
        } else {
            hasher.update(TO_SERVER_MAGIC);
            hasher.update(&self.to_server_counter.to_be_bytes());
        }
        hasher
    }

    pub fn read_app_data<'b>(&mut self, record: &'b mut [u8]) -> Result<&'b [u8]> {
        if &record[..3] != &[RECORD_APPLICATION_DATA, 0x03, 0x03] {
            return Err(anyhow!(
                "[{}]reject: restls application data must have 0x17 header, got: {:?}",
                self.id,
                record
            ));
        }
        let actual_auth = &record[5..5 + RESTLS_APPDATA_HMAC_LEN];
        let mut hmac_auth = self.restls_appdata_auth_hmac(false);
        if self.client_finished.len() > 0 {
            debug!("adding client_finished {:?}", self.client_finished);
            hmac_auth.update(&self.client_finished);
            self.client_finished.resize(0, 0);
        }
        hmac_auth.update(&record[RESTLS_APPDATA_LEN_OFFSET..]);
        let expect_auth = hmac_auth.finalize().into_bytes();
        if actual_auth != &expect_auth[..RESTLS_APPDATA_HMAC_LEN] {
            debug!(
                "[{}]bad mac record, expect auth {:?}, actual {:?}, to_client: {}, to_server: {}",
                self.id,
                &expect_auth[..RESTLS_APPDATA_HMAC_LEN],
                actual_auth,
                self.to_client_counter,
                self.to_server_counter
            );
            return Err(anyhow!("reject: bad mac record"));
        }

        let mut hmac_mask = self.restls_appdata_auth_hmac(false);
        hmac_mask.update(sample_slice(&record[RESTLS_APPDATA_OFFSET..]));
        let mask = hmac_mask.finalize().into_bytes();
        let data_len_bytes = &mut record[RESTLS_APPDATA_LEN_OFFSET..][..2];
        xor_bytes(&mask[..2], data_len_bytes);
        let data_len = (data_len_bytes[0] as usize) << 8 | (data_len_bytes[1] as usize);
        self.to_server_counter += 1;
        debug!("[{}]read_app_data: data_len {}", self.id, data_len);
        Ok(&record[RESTLS_APPDATA_OFFSET..][..data_len])
    }

    pub fn write_app_data_header<'b>(
        &mut self,
        record: &'b mut [u8],
        data_len: usize,
    ) -> Result<(usize, usize)> {
        assert!(record.len() >= 1500);
        let mtu = (self.mtu as isize + rand::thread_rng().gen_range(-50..50))
            .try_into()
            .unwrap();
        let min_record_len = self.min_record_len + rand::thread_rng().gen_range(0..100);
        let (real_data_len, padding) = match (
            data_len < min_record_len,
            self.to_client_raw_counter < 5,
            data_len > mtu,
        ) {
            (true, _, _) => (data_len, min_record_len - data_len),
            (_, false, _) => (data_len, 0),
            (_, true, true) => (mtu, 0),
            (_, true, false) => (data_len, mtu - data_len),
        };
        if padding > 0 {
            rand::thread_rng()
                .fill(&mut record[RESTLS_APPDATA_OFFSET + real_data_len..][..padding]);
        }
        let mut hmac_mask = self.restls_appdata_auth_hmac(true);
        hmac_mask.update(sample_slice(
            &record[RESTLS_APPDATA_OFFSET..][..real_data_len + padding],
        ));
        let mask = hmac_mask.finalize().into_bytes();
        record[RESTLS_APPDATA_LEN_OFFSET..][..2]
            .copy_from_slice(&(real_data_len as u16).to_be_bytes());
        xor_bytes(&mask[..2], &mut record[RESTLS_APPDATA_LEN_OFFSET..]);
        let mut hmac_auth = self.restls_appdata_auth_hmac(true);
        hmac_auth.update(&record[RESTLS_APPDATA_LEN_OFFSET..][..2 + real_data_len + padding]);
        let auth = hmac_auth.finalize().into_bytes();
        record[5..5 + RESTLS_APPDATA_HMAC_LEN].copy_from_slice(&auth[..RESTLS_APPDATA_HMAC_LEN]);
        record[0..3].copy_from_slice(&[0x17, 0x3, 0x3]);
        record[3..5].copy_from_slice(&((real_data_len + padding + 10) as u16).to_be_bytes());
        self.to_client_counter += 1;
        if real_data_len == data_len {
            self.to_client_raw_counter += 1;
        }
        debug!(
            "[{}]write_header: data_len {}, padding: {}, mask: {:?}, auth: {:?}",
            self.id,
            real_data_len,
            padding,
            &mask[..2],
            &auth[..RESTLS_APPDATA_HMAC_LEN]
        );
        Ok((real_data_len, real_data_len + padding + 15))
    }

    async fn read_from_stream(&self, stream: &mut TLSStream) -> Result<()> {
        if stream.codec().has_next() {
            Ok(())
        } else {
            match stream.next().await {
                None => Err(anyhow!("unexpected eof")),
                Some(res) => res,
            }
        }
    }

    async fn try_read_client_hello(&mut self, inbound: &mut TLSStream) -> Result<()> {
        match self.read_from_stream(inbound).await {
            Err(e) => return Err(e.context("failed to read client hello: ")),
            Ok(()) => (),
        };
        let rtype = inbound.codec().peek_record_type()?;
        if rtype != RECORD_HANDSHAKE {
            return Err(anyhow!(
                "reject: incorrect record type for client hello, actual: {}",
                rtype
            ));
        }
        let record = inbound
            .codec_mut()
            .next_record()
            .expect("unexpected error: record has been checked");
        let mut cursor = Cursor::new(&*record);
        self.client_hello =
            Some(ClientHello::parse(&mut cursor).context("unable to parse client hello: ")?);
        Ok(())
    }

    async fn try_read_server_hello(&mut self, outbound: &mut TLSStream) -> Result<()> {
        self.read_from_stream(outbound)
            .await
            .context("failed to read server hello: ")?;
        let rtype = outbound.codec().peek_record_type()?;
        if rtype != RECORD_HANDSHAKE {
            return Err(anyhow!(
                "reject: incorrect record type for server hello, actual: {}",
                rtype
            ));
        }
        let record = outbound
            .codec_mut()
            .next_record()
            .expect("unexpected error: record has been checked");
        let mut cursor = Cursor::new(&*record);
        self.server_hello =
            Some(ServerHello::parse(&mut cursor).context("unable to parse client hello: ")?);
        Ok(())
    }

    async fn try_read_tls13_till_first_0x17(
        &mut self,
        outbound: &mut TLSStream,
        inbound: &mut TLSStream,
    ) -> Result<()> {
        let mut ccs_from_server = false;
        loop {
            self.read_from_stream(outbound).await?;
            let rtype = outbound.codec().peek_record_type()?;

            match rtype {
                RECORD_CCS if !ccs_from_server => {
                    ccs_from_server = true;
                }
                RECORD_APPLICATION_DATA if ccs_from_server => {
                    break;
                }
                _ => {
                    return Err(anyhow!(
                    "reject: incorrect outbound tls13 record type, expected 1 CCS or Application Data, actual {rtype}",
                ))
                }
            }
            outbound
                .codec_mut()
                .next_record()
                .expect("unexpected error: record has been checked");
            self.relay_to(inbound, outbound).await?;
        }

        Ok(())
    }

    fn prepare_server_auth(&self, outbound: &mut TLSStream) {
        let mut hasher = self.restls_hmac();
        hasher.update(&self.server_hello.as_ref().unwrap().server_random);
        let secret = hasher.finalize().into_bytes();
        debug!(
            "[{}]server challenge {:?}",
            self.id,
            &secret[..RESTLS_HANDSHAKE_HMAC_LEN]
        );
        let record = outbound
            .codec_mut()
            .peek_record_mut()
            .expect("unexpected error: record has been checked");
        xor_bytes(&secret[..RESTLS_HANDSHAKE_HMAC_LEN], &mut record[5..]);
    }

    async fn try_read_tl13_till_client_application_data(
        &mut self,
        outbound: &mut TLSStream,
        inbound: &mut TLSStream,
    ) -> Result<()> {
        let mut seen_client_application_data = 0;
        let mut ccs_from_client = false;
        loop {
            select! {
                res = self.read_from_stream(inbound) => {
                    let _ = res?;
                    match inbound.codec().peek_record_type()? {
                        RECORD_CCS if !ccs_from_client => {
                            ccs_from_client = true;
                        }
                        RECORD_APPLICATION_DATA if ccs_from_client => {
                            seen_client_application_data += 1;
                            if seen_client_application_data == 1 {
                                self.client_finished.extend_from_slice(inbound.codec().peek_record().unwrap());
                            } else if seen_client_application_data == 2 {
                                break;
                            }
                        }
                        rtype => {
                            return Err(anyhow!(
                                "reject: incorrect inbound tls13 record type, expected 1 CCS or Application Data, actual {rtype}",
                            ));
                        }
                    }
                    inbound.codec_mut().next_record().expect("unexpected error: record has been checked");
                    self.relay_to(outbound, inbound).await?;
                }
                res = self.read_from_stream(outbound) => {
                    let _ = res?;
                    outbound.codec_mut().skip_to_end();
                    self.relay_to(inbound, outbound).await?;
                }
            }
        }
        Ok(())
    }

    fn check_tls13_session_id(&self) -> Result<()> {
        let mut hasher = self.restls_hmac();
        let client_hello = self.client_hello.as_ref().unwrap();
        hasher.update(&client_hello.key_share);
        hasher.update(&client_hello.psk);
        let res_raw = hasher.finalize();
        let res = res_raw.into_bytes();
        let expect = &res[..RESTLS_HANDSHAKE_HMAC_LEN];
        let actual = &client_hello.session_id[..RESTLS_HANDSHAKE_HMAC_LEN];
        // we don't need constant time comparison since
        // that's not noticeable compared to network delay.
        if expect == actual {
            Ok(())
        } else {
            Err(anyhow!(
                "reject: incorrect session id, expect: {:?}, actual {:?}",
                expect,
                actual
            ))
        }
    }

    fn handle_tls12_outbound(&self, outbound: &mut TLSStream, flow: &mut TLS12Flow) -> Result<()> {
        let rtype = outbound.codec().peek_record_type()?;
        match rtype {
            RECORD_CCS => {
                flow.ccs_from_server()?;
            }
            RECORD_HANDSHAKE if flow.is_ccs_from_server() => {
                if flow.is_resume() {
                    self.check_tls12_session_ticket()?;
                }
                self.prepare_server_auth(outbound);
                debug!("[{}]sending tls12 server auth to client", self.id);
            }
            RECORD_HANDSHAKE => (),
            _ => {
                return Err(anyhow!(
                    "reject: incorrect outbound tls12 record type, expected 1 CCS or Handshake, actual {rtype}",
                ))
            }
        }
        Ok(())
    }

    fn handle_tls12_inbound(&mut self, inbound: &TLSStream, flow: &mut TLS12Flow) -> Result<()> {
        let rtype = inbound.codec().peek_record_type()?;
        match rtype {
            RECORD_CCS => {
                flow.ccs_from_client()?;
                Ok(())
            }
            RECORD_HANDSHAKE if flow.expect_cke() => {
                // We expect CKE to be the first 0x16 after client hello.
                let maybe_cke = inbound
                    .codec()
                    .peek_record()
                    .expect("unexpected error: record has been checked");
                ClientKeyExchange::check(
                    &mut Cursor::new(maybe_cke),
                    self.restls_password,
                    self.client_hello.as_ref().unwrap(),
                )?;
                flow.cke_verified()
            }
            RECORD_HANDSHAKE if flow.is_resume_ccs_from_client() => {
                // For tls 1.2 w/ resume, we need to hash the client finished.
                self.client_finished
                    .extend_from_slice(inbound.codec().peek_record().unwrap());

                Ok(())
            }
            RECORD_HANDSHAKE => Ok(()),
            RECORD_APPLICATION_DATA => flow.client_0x17(),
            _ => Err(anyhow!(
                "reject: incorrect tls12 inbound record type, expected 1 CCS or Handshake, actual {rtype}",
            )),
        }
    }

    async fn try_read_tls12_till_client_application_data(
        &mut self,
        outbound: &mut TLSStream,
        inbound: &mut TLSStream,
    ) -> Result<()> {
        let mut flow = TLS12Flow::Initial;
        loop {
            debug!("[{}]flow {:?}", self.id, flow);
            select! {
                ret = self.read_from_stream(outbound) => {
                    match ret {
                        Err(_) => return Ok(()),
                        _ => (),
                    }
                    self.handle_tls12_outbound(outbound, &mut flow)?;
                    outbound
                        .codec_mut()
                        .next_record()
                        .expect("unexpected error: record has been checked");
                    self.relay_to(inbound, outbound).await?;
                }
                ret = self.read_from_stream(inbound) => {
                    ret?;
                    self.handle_tls12_inbound(inbound, &mut flow)?;
                    if flow.is_client_0x17() {
                        break;
                    }
                    inbound
                        .codec_mut()
                        .next_record()
                        .expect("unexpected error: record has been checked");
                    self.relay_to(outbound, inbound).await?;
                }
            }
        }
        Ok(())
    }

    fn check_tls12_session_ticket(&self) -> Result<()> {
        let mut hasher = self.restls_hmac();
        let client_hello = self.client_hello.as_ref().unwrap();
        hasher.update(&client_hello.session_ticket);
        let actual_hash = hasher.finalize().into_bytes();
        if &client_hello.session_id[RESTLS_HANDSHAKE_HMAC_LEN..]
            != &actual_hash[..RESTLS_HANDSHAKE_HMAC_LEN]
        {
            Err(anyhow!("reject: tls 1.2 client pub key mismatched"))
        } else {
            Ok(())
        }
    }

    async fn relay_to(
        &mut self,
        to_stream: &mut TLSStream,
        from_stream: &mut TLSStream,
    ) -> Result<()> {
        if from_stream.codec().has_next() {
            return Ok(());
        }
        let res = match to_stream
            .get_mut()
            .write_all(&from_stream.codec().raw_buf())
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => Err(e.into()),
        };
        from_stream.codec_mut().reset();
        res
    }

    pub async fn copy_bidirectional(
        &mut self,
        inbound: &mut TLSStream,
        outbound: &mut TcpStream,
    ) -> Result<()> {
        let mut out_buf = [0; BUF_SIZE];
        loop {
            select! {
                res = self.read_from_stream(inbound) => {
                    res?;
                    while inbound.codec().has_next() {
                        let record = inbound.codec_mut().next_record()?;
                        if record[0] == RECORD_ALERT {
                            return Ok(());
                        }
                        let record = match self.read_app_data(record) {
                            Ok(record) => record,
                            Err(e) => {
                                return if self.server_hello.as_ref().unwrap().is_tls13
                                    && self.to_client_counter > 0
                                    && self.to_server_counter > 0
                                {
                                    // this will probably be a close notify.
                                    // we'll ignore it.
                                    Ok(())
                                } else {
                                    Err(e)
                                }
                            }
                        };
                        if record.len() > 0 {
                            outbound.write_all(record).await.context("outbound.write_all failed: ")?;
                        }
                    }
                    inbound.codec_mut().reset();
                }
                n = outbound.read(&mut out_buf[RESTLS_APPDATA_OFFSET..]) => {
                    let mut n = n.context("outbound.read failed: ")?;
                    if n == 0 {
                        return Ok(());
                    }
                    let mut written = 0;
                    while written < n {
                        if (BUF_SIZE - written) < 1500 {
                            for i in 0..BUF_SIZE - written {
                                out_buf[i] = out_buf[written + i];
                            }
                            n -= written;
                            written = 0;
                        }
                        let (new_written, packet_size) = self.write_app_data_header(&mut out_buf[written..], n - written)?;
                        inbound.get_mut().write_all(&out_buf[written..][..packet_size]).await.context("inbound.write_all failed: ")?;
                        written += new_written;
                    }
                }
            }
        }
    }

    async fn try_handshake(
        &mut self,
        outbound: &mut TLSStream,
        inbound: &mut TLSStream,
    ) -> Result<()> {
        self.try_read_client_hello(inbound).await?;
        self.relay_to(outbound, inbound).await?;

        self.try_read_server_hello(outbound).await?;
        self.relay_to(inbound, outbound).await?;

        if self.server_hello.as_ref().unwrap().is_tls13 {
            self.check_tls13_session_id()?;
            self.try_read_tls13_till_first_0x17(outbound, inbound)
                .await?;
            self.prepare_server_auth(outbound);
            outbound.codec_mut().next_record().unwrap();
            debug!("[{}]sending tls13 server auth to client", self.id);
            self.relay_to(inbound, outbound).await?;
            self.try_read_tl13_till_client_application_data(outbound, inbound)
                .await?;
        } else {
            self.try_read_tls12_till_client_application_data(outbound, inbound)
                .await?;
        }
        Ok(())
    }
}

pub async fn handle(options: Arc<Opt>, inbound: TcpStream, id: usize) -> Result<()> {
    let mut outbound = TLSCodec::new_inbound().framed(
        // TcpStream::connect("89.145.65.200:443")
        TcpStream::connect(&options.server_hostname)
            .await
            .context("cannot connect to outbound".to_owned() + &options.server_hostname)?,
    );

    let mut inbound = TLSCodec::new_outbound().framed(inbound);
    let mut try_handshake = RestlsState {
        client_hello: None,
        server_hello: None,
        restls_password: options.password.as_bytes(),
        client_finished: Vec::new(),
        to_client_counter: 0,
        to_server_counter: 0,
        to_client_raw_counter: 0,
        mtu: options.mtu as usize,
        min_record_len: options.mtu as usize,
        id,
    };
    match try_handshake
        .try_handshake(&mut outbound, &mut inbound)
        .await
    {
        Ok(()) => {
            let mut outbound_proxy = TcpStream::connect(&options.forward_to).await?;
            match try_handshake
                .copy_bidirectional(&mut inbound, &mut outbound_proxy)
                .await
            {
                Err(e) => {
                    tracing::error!("restls data relay failed: {}", e);
                    tcp_rst(inbound.get_mut()).await?
                }
                _ => (),
            }
        }
        Err(e) => {
            tracing::error!("handshake failed: {}", e);
            copy_bidirectional_fallback(inbound, outbound).await?;
        }
    }
    Ok(())
}

pub async fn start(options: Arc<Opt>) -> Result<()> {
    let listener = TcpListener::bind(&options.listen).await?;

    tracing::info!(
        "Restls server started as {} on {}, forwarding to {}",
        options.server_hostname,
        options.listen,
        options.forward_to,
    );
    let mut counter = 0;
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("failed to accept inbound stream")?;
        let options = options.clone();

        tokio::spawn(async move {
            match handle(options, stream, counter).await {
                Err(e) => tracing::debug!("[{}]{}", counter, e),
                Ok(_) => (),
            }
        });
        counter += 1;
    }
}
