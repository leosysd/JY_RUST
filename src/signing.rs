use anyhow::{bail, Result};
use k256::ecdsa::{signature::Signer, SigningKey};
use sha3::{Digest, Keccak256};

const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";

const ORDER_TYPE_HASH: &[u8] = b"Order(uint256 salt,address maker,address signer,\
address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,\
uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)";

const DOMAIN_TYPE_HASH: &[u8] = b"EIP712Domain(string name,string version,\
uint256 chainId,address verifyingContract)";

pub struct OrderSigner {
    key: SigningKey,
    pub address: [u8; 20],
    chain_id: u64,
}

#[derive(Debug)]
pub struct OrderParams {
    pub token_id: String,
    pub price: f64,
    pub size: f64,
    pub side: u8,          // 0=BUY 1=SELL
    pub maker: [u8; 20],   // deposit wallet
    pub sig_type: u8,      // 0=EOA 2=POLY_PROXY
    pub neg_risk: bool,
    pub tick_size: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct SignedOrder {
    pub salt: u64,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    #[serde(rename = "tokenId")]
    pub token_id: String,
    #[serde(rename = "makerAmount")]
    pub maker_amount: String,
    #[serde(rename = "takerAmount")]
    pub taker_amount: String,
    pub expiration: String,
    pub nonce: String,
    #[serde(rename = "feeRateBps")]
    pub fee_rate_bps: String,
    pub side: String,
    #[serde(rename = "signatureType")]
    pub signature_type: String,
    pub signature: String,
}

impl OrderSigner {
    pub fn new(private_key_hex: &str, chain_id: u64) -> Result<Self> {
        let hex = private_key_hex.trim_start_matches("0x");
        let bytes = hex::decode(hex)?;
        if bytes.len() != 32 {
            bail!("invalid private key length");
        }
        let key = SigningKey::from_bytes(bytes.as_slice().into())?;
        let verifying = key.verifying_key();
        let pubkey_bytes = verifying.to_encoded_point(false);
        let pubkey_uncompressed = &pubkey_bytes.as_bytes()[1..]; // remove 0x04 prefix
        let hash = keccak256(pubkey_uncompressed);
        let mut address = [0u8; 20];
        address.copy_from_slice(&hash[12..]);
        Ok(Self { key, address, chain_id })
    }

    pub fn sign_order(&self, params: &OrderParams) -> Result<SignedOrder> {
        let salt: u64 = rand::random();
        let token_id: u128 = params.token_id.parse().unwrap_or(0);
        let (maker_amount, taker_amount) = self.compute_amounts(params);

        let domain_sep = self.domain_separator();
        let struct_hash = self.order_struct_hash(
            salt, &params.maker, &self.address,
            token_id, maker_amount, taker_amount,
            params.side, params.sig_type,
        );

        let mut digest_input = [0u8; 66];
        digest_input[0] = 0x19;
        digest_input[1] = 0x01;
        digest_input[2..34].copy_from_slice(&domain_sep);
        digest_input[34..66].copy_from_slice(&struct_hash);
        let digest = keccak256(&digest_input);

        let sig: k256::ecdsa::Signature = self.key.sign(&digest);
        let (r, s) = (sig.r(), sig.s());
        let recovery = self.recovery_id(&digest, &sig)?;
        let v = recovery + 27;

        let mut sig_bytes = [0u8; 65];
        sig_bytes[..32].copy_from_slice(&r.to_bytes());
        sig_bytes[32..64].copy_from_slice(&s.to_bytes());
        sig_bytes[64] = v;

        Ok(SignedOrder {
            salt,
            maker: addr_hex(&params.maker),
            signer: addr_hex(&self.address),
            taker: "0x0000000000000000000000000000000000000000".into(),
            token_id: params.token_id.clone(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: "0".into(),
            nonce: "0".into(),
            fee_rate_bps: "0".into(),
            side: params.side.to_string(),
            signature_type: params.sig_type.to_string(),
            signature: format!("0x{}", hex::encode(sig_bytes)),
        })
    }

    fn compute_amounts(&self, p: &OrderParams) -> (u128, u128) {
        let usdc_dec = 1_000_000u128;
        let shares_dec = 1_000_000u128;
        let size = (p.size * 1e6).round() as u128;
        let cost = (p.size * p.price * 1e6).round() as u128;
        if p.side == 0 {
            // BUY: maker pays USDC, taker gives shares
            (cost * usdc_dec / 1_000_000, size * shares_dec / 1_000_000)
        } else {
            // SELL: maker gives shares, taker pays USDC
            (size * shares_dec / 1_000_000, cost * usdc_dec / 1_000_000)
        }
    }

    fn domain_separator(&self) -> [u8; 32] {
        let domain_type_hash = keccak256(DOMAIN_TYPE_HASH);
        let name_hash = keccak256(b"CTFExchange");
        let version_hash = keccak256(b"1");
        let contract = parse_addr(CTF_EXCHANGE);

        let mut buf = [0u8; 5 * 32];
        buf[0..32].copy_from_slice(&domain_type_hash);
        buf[32..64].copy_from_slice(&name_hash);
        buf[64..96].copy_from_slice(&version_hash);
        buf[108..128].copy_from_slice(&self.chain_id.to_be_bytes()[..]);  // right-aligned in 32 bytes
        // chain_id u64 = 8 bytes, padded left to 32
        let chain_bytes = self.chain_id.to_be_bytes();
        buf[96..128].fill(0);
        buf[120..128].copy_from_slice(&chain_bytes);
        buf[128 + 12..160].copy_from_slice(&contract);

        keccak256(&buf)
    }

    fn order_struct_hash(
        &self, salt: u64, maker: &[u8; 20], signer: &[u8; 20],
        token_id: u128, maker_amount: u128, taker_amount: u128,
        side: u8, sig_type: u8,
    ) -> [u8; 32] {
        let type_hash = keccak256(ORDER_TYPE_HASH);
        let taker = [0u8; 20];

        let mut buf = [0u8; 12 * 32];
        buf[0..32].copy_from_slice(&type_hash);
        write_u256(&mut buf[32..64], salt as u128);
        write_addr(&mut buf[64..96], maker);
        write_addr(&mut buf[96..128], signer);
        write_addr(&mut buf[128..160], &taker);
        write_u256(&mut buf[160..192], token_id);
        write_u256(&mut buf[192..224], maker_amount);
        write_u256(&mut buf[224..256], taker_amount);
        write_u256(&mut buf[256..288], 0); // expiration
        write_u256(&mut buf[288..320], 0); // nonce
        write_u256(&mut buf[320..352], 0); // feeRateBps
        write_u256(&mut buf[352..384], side as u128);
        // sig_type would be the 13th field but struct only has 12 fields listed
        let _ = sig_type;

        keccak256(&buf)
    }

    fn recovery_id(&self, digest: &[u8; 32], sig: &k256::ecdsa::Signature) -> Result<u8> {
        use k256::ecdsa::RecoveryId;
        for v in [0u8, 1u8] {
            let rid = RecoveryId::from_byte(v).unwrap();
            if let Ok(recovered) = k256::ecdsa::VerifyingKey::recover_from_prehash(digest, sig, rid) {
                if recovered == *self.key.verifying_key() {
                    return Ok(v);
                }
            }
        }
        bail!("cannot determine recovery id")
    }
}

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn write_u256(buf: &mut [u8], v: u128) {
    buf.fill(0);
    buf[16..32].copy_from_slice(&v.to_be_bytes());
}

fn write_addr(buf: &mut [u8], addr: &[u8; 20]) {
    buf.fill(0);
    buf[12..32].copy_from_slice(addr);
}

fn addr_hex(addr: &[u8; 20]) -> String {
    format!("0x{}", hex::encode(addr))
}

fn parse_addr(s: &str) -> [u8; 20] {
    let h = s.trim_start_matches("0x");
    let b = hex::decode(h).unwrap_or_default();
    let mut out = [0u8; 20];
    if b.len() == 20 { out.copy_from_slice(&b); }
    out
}
