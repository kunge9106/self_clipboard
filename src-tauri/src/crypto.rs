//! 加密内核：密钥文件 `key.bin` + AES-256-GCM 容器（v2）。
//!
//! 这一层只负责两件事：**怎么保管一把 32 字节的密钥**、**怎么把一段字节封进容器再取出来**。
//! 它不认识索引、不认识历史条目，也不做任何 IO 编排 —— 谁用谁决定写到哪。
//!
//! 为什么要从「整份 DPAPI」改到「DPAPI 包密钥 + AES-GCM 包数据」：
//! `CryptProtectData` 的耗时不是线性的，实测 0.5 MB 要 21 ms、4.7 MB 要 207 ms，
//! 更像存在 200 ms 量级的固定开销。十万条历史下每次复制都整份重写一遍索引，一次就是
//! 三百多毫秒，连复制几次就积压。把 DPAPI 收敛到「进程启动时解一次密钥」，写盘路径上
//! 就不再有任何系统调用级的加密开销，而 AES-NI 加密 38 MB 只要 25 ms 量级。
//!
//! 容器里为什么要绑 `域串 + 相对路径`（AAD）：
//! 成本为零（AAD 不占体积、不算密文），但密文从此不能「搬家」——
//! 把 `data/a.txt` 的密文原样写到 `data/b.txt`、或者把索引密文塞进实体文件，
//! 都会在解密时直接校验失败。否则攻击者可以把「受保护的正文」换成「另一条受保护的正文」，
//! 每个文件单独看都合法，拼在一起就是错的。
//!
//! 不自己实现密码学原语：AES/GCM 来自 `aes-gcm`（RustCrypto），随机数来自 `getrandom`
//! （Windows 上走 `BCryptGenRandom`）。自己写 AES 与自造随机源是这类模块唯一无法通过
//! 单测兜底的错误来源。
//!
//! DPAPI 的绑定为什么在本模块里自带一份 FFI（而不是复用 `clipboard` 的那份）：
//! ① 两者熵不同 —— 密钥用 `qingtie.key.v1`，正文用 `qingtie.clipboard.v1`，熵决定
//!    「同账户下别的进程即便调 DPAPI 也解不开」，混用等于把两套保护降级成一套；
//! ② 本模块必须能脱离剪贴板那一整套 Win32 声明独立编译与单测（容器部分是纯 Rust）。
//! 重复的只有一个 `DATA_BLOB` 与三个 `extern` 声明，比反向依赖一个六千行模块更划算。

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};

// ---------------------------------------------------------------------------
// 密钥文件
// ---------------------------------------------------------------------------

/// 密钥文件名。固定放在缓存目录根，跟着目录整体迁移与备份。
pub const KEY_FILE_NAME: &str = "key.bin";

/// 密钥文件魔数。首字节是 `\0`，用户文本几乎不可能以 NUL 开头，因此这个前缀
/// 与「历史遗留的其它文件」不可能混淆。
pub const KEY_MAGIC: &[u8] = b"\0QTEK\x01";

/// 密钥明文头的长度：魔数 6 + 保留 2 + key_id 8 + created_ms 8。
///
/// `key_id` 用 8 字节随机数而不是「密钥哈希的前 4 字节」：容器头里也要写它，
/// 8 字节的碰撞概率在密钥轮换场景下足够低，且不用引入哈希依赖。
/// 中间那 2 字节保留位是给「将来要在头里加字段」留的落地位置 —— 改头长度会让
/// 已落盘的密钥文件全部失效。
pub const KEY_HEADER_LEN: usize = 24;

/// AES-256 的密钥长度。
pub const KEY_LEN: usize = 32;

/// 密钥文件的 DPAPI 熵。改它等于让所有已落盘的 `key.bin` 解不开，不要动。
const KEY_ENTROPY: &[u8] = b"qingtie.key.v1";

// ---------------------------------------------------------------------------
// 容器（v2）
// ---------------------------------------------------------------------------

/// 容器魔数。与 v1 的 `\0QTE1\x01` 只差一个字节，靠它分派「AES 容器 / DPAPI 整份 / 明文 JSON」。
pub const MAGIC_V2: &[u8] = b"\0QTE2\x01";

/// v1（整份 DPAPI）的魔数，仅供格式识别使用。
pub const MAGIC_V1: &[u8] = b"\0QTE1\x01";

/// 算法编号：0x01 = AES-256-GCM。留出编号空间是为了将来换算法时能原地判别。
const ALG_AES_256_GCM: u8 = 0x01;

/// AAD 只用容器头的前 32 字节（魔数 + 算法 + key_id + write_seq + plain_len）。
/// nonce 与 tag 之后才出现，且 nonce 本身已作为 IV 参与 tag 计算，放进 AAD 是多余的。
const AAD_HEADER_LEN: usize = 32;

const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// 密文前面的固定前缀长度：32 头 + 12 nonce + 16 tag。
pub const CONTAINER_PREFIX_LEN: usize = AAD_HEADER_LEN + NONCE_LEN + TAG_LEN;

const KEY_ID_OFFSET: usize = 8;
const WRITE_SEQ_OFFSET: usize = 16;
const PLAIN_LEN_OFFSET: usize = 24;
const NONCE_OFFSET: usize = 32;
const TAG_OFFSET: usize = 44;

/// 域串：索引快照。
pub const DOMAIN_INDEX: &[u8] = b"index";
/// 域串：`data/<id>.txt` 这类正文实体。
pub const DOMAIN_TEXT: &[u8] = b"text";
/// 域串：`files/<id>.txt` 这类路径清单实体。
pub const DOMAIN_FILES: &[u8] = b"files";

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 打开容器失败的四种原因。**分开而不是合成一个** `Err`：调用方要据此分派
/// 完全不同的后续动作（走旧格式兼容 / 判损坏重建 / 报「用错了密钥」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// 不是 v2 容器（旧格式）。调用方应回退到 v1 DPAPI 或明文兼容路径。
    NotV2,
    /// 结构不合法：长度不足、算法未知、`plain_len` 与实际密文长度不符。
    Malformed,
    /// GCM 校验失败：密文/tag/头被改过，或被搬到了别的域、别的路径。
    Tampered,
    /// 容器是别的密钥写的（`key_id` 与当前密钥不一致）。
    WrongKey,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 面向日志的中文短句：出问题时用户会把日志贴过来，这里要能直接看懂。
        let text = match self {
            CryptoError::NotV2 => "不是 v2 加密容器（旧格式）",
            CryptoError::Malformed => "加密容器结构损坏（长度或字段不合法）",
            CryptoError::Tampered => "加密容器校验失败（内容被改动或位置被搬动）",
            CryptoError::WrongKey => "加密容器用的是另一把密钥",
        };
        f.write_str(text)
    }
}

impl std::error::Error for CryptoError {}

/// 磁盘上一段字节的格式判定。给迁移逻辑用：先判格式，再决定拿哪条路解。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// v2 AES-GCM 容器。
    V2,
    /// v1 整份 DPAPI。
    V1Dpapi,
    /// v0 明文 JSON（首字符 `{` 或 `[`，允许前面有空白）。
    V0Plaintext,
    /// 空文件或认不出来的东西。
    Unknown,
}

/// 按魔数/首字符判定格式。
///
/// 只做分类，不做完整性校验 —— 判成 `V2` 不等于一定能解开，解开时才知道。
pub fn detect(bytes: &[u8]) -> Format {
    if bytes.starts_with(MAGIC_V2) {
        Format::V2
    } else if bytes.starts_with(MAGIC_V1) {
        Format::V1Dpapi
    } else {
        match bytes.iter().find(|b| !b.is_ascii_whitespace()) {
            Some(b'{') | Some(b'[') => Format::V0Plaintext,
            _ => Format::Unknown,
        }
    }
}

// ---------------------------------------------------------------------------
// 密钥
// ---------------------------------------------------------------------------

/// 一把已解封的密钥。只活在内存里，`Debug` 打不出密钥字节。
pub struct Key {
    bytes: [u8; KEY_LEN],
    key_id: u64,
    created_ms: u64,
}

impl Key {
    /// 密钥标识。写进每个容器头，用来区分「用错了密钥」与「内容被改过」。
    pub fn key_id(&self) -> u64 {
        self.key_id
    }

    /// 密钥创建时间（Unix 毫秒）。只用于诊断日志。
    pub fn created_ms(&self) -> u64 {
        self.created_ms
    }

    fn cipher(&self) -> Aes256Gcm {
        // `bytes` 是定长 `[u8; KEY_LEN]`，长度恒为 32，这里除了「类型系统被改坏」
        // 没有别的失败可能。
        Aes256Gcm::new_from_slice(&self.bytes).expect("AES-256 密钥长度恒为 32 字节")
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 密钥字节绝不进日志、绝不进 panic 信息。
        write!(f, "Key(key_id={:016x}, bytes=***)", self.key_id)
    }
}

/// `key.bin` 的完整路径。缓存目录可能被用户改到别处，所以路径永远是「目录 + 文件名」算出来的。
pub fn key_path(dir: &Path) -> PathBuf {
    dir.join(KEY_FILE_NAME)
}

/// 读密钥；不存在则以 `getrandom` 生成一把并落盘。
///
/// **文件存在但解不开时返回错误，不生成新密钥。** 这条是硬约束：静默换钥匙意味着
/// 已落盘的所有数据当场变成永久乱码，而用户只看到「历史都没了」。到底重建还是提示，
/// 是调用方的决定（可保留旧文件、可提示原因），不是本函数的。
pub fn load_or_create_key(dir: &Path) -> io::Result<Key> {
    let path = key_path(dir);
    match fs::read(&path) {
        Ok(raw) => decode_key_file(&raw),
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_key_file(dir, &path),
        Err(e) => Err(e),
    }
}

/// 按密钥文件布局解出密钥。
fn decode_key_file(raw: &[u8]) -> io::Result<Key> {
    if raw.len() < KEY_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "密钥文件长度不足（{} 字节，至少 {KEY_HEADER_LEN} 字节），按损坏处理；不会自动重建",
                raw.len()
            ),
        ));
    }
    if &raw[..KEY_MAGIC.len()] != KEY_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "密钥文件魔数不匹配，按损坏处理；不会自动重建",
        ));
    }
    let key_id = read_u64(raw, KEY_ID_OFFSET);
    let created_ms = read_u64(raw, KEY_ID_OFFSET + 8);
    let blob = &raw[KEY_HEADER_LEN..];
    let plain = keyring::unprotect(blob, KEY_ENTROPY).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "密钥文件的 DPAPI blob 解不开（换了 Windows 账户或机器）；不会自动重建",
        )
    })?;
    if plain.len() != KEY_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("密钥明文长度应为 {KEY_LEN} 字节，实际 {} 字节", plain.len()),
        ));
    }
    let mut bytes = [0u8; KEY_LEN];
    bytes.copy_from_slice(&plain);
    Ok(Key {
        bytes,
        key_id,
        created_ms,
    })
}

/// 生成一把新密钥并原子落盘（临时文件 → `sync_all` → rename）。
///
/// 先 `sync_all` 再 rename：断电时要么看到完整的旧文件，要么看到完整的新文件，
/// 不会出现「半截密钥文件」，而半截密钥文件等于丢密钥。
fn create_key_file(dir: &Path, path: &Path) -> io::Result<Key> {
    fs::create_dir_all(dir)?;

    let mut bytes = [0u8; KEY_LEN];
    random_bytes(&mut bytes)?;
    let mut id_bytes = [0u8; 8];
    random_bytes(&mut id_bytes)?;
    let key_id = u64::from_le_bytes(id_bytes);
    let created_ms = now_ms();

    let blob = keyring::protect(&bytes, KEY_ENTROPY).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            "DPAPI 加密密钥失败，密钥未落盘（本次不写任何数据）",
        )
    })?;

    let mut out = Vec::with_capacity(KEY_HEADER_LEN + blob.len());
    out.extend_from_slice(KEY_MAGIC);
    out.extend_from_slice(&[0u8, 0u8]); // 保留位：将来加字段用，当前恒 0
    out.extend_from_slice(&key_id.to_le_bytes());
    out.extend_from_slice(&created_ms.to_le_bytes());
    out.extend_from_slice(&blob);

    let tmp = dir.join(format!("{KEY_FILE_NAME}.tmp"));
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&out)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;

    Ok(Key {
        bytes,
        key_id,
        created_ms,
    })
}

// ---------------------------------------------------------------------------
// 封包 / 解包
// ---------------------------------------------------------------------------

/// 把明文封进 v2 容器。
///
/// `write_seq` 只对「索引快照」有意义（单调递增，用于发现「加载到的快照比上次还旧」
/// 这种回滚重放）；实体一律传 0。它进 AAD，改它会导致校验失败。
///
/// 返回整个容器（头 + nonce + tag + 密文）。加密本身不会失败；随机数取不到时直接
/// 中止进程 —— AES-GCM 的 nonce 一旦复用，两段明文的异或会直接暴露，宁可崩也不能猜。
pub fn seal(
    key: &Key,
    domain: &[u8],
    rel_path: Option<&str>,
    plain: &[u8],
    write_seq: u64,
) -> Vec<u8> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    if let Err(e) = random_bytes(&mut nonce_bytes) {
        panic!("系统随机数不可用（{e}）：拒绝用可预测的 nonce 继续加密");
    }

    let mut out = vec![0u8; CONTAINER_PREFIX_LEN + plain.len()];
    out[..MAGIC_V2.len()].copy_from_slice(MAGIC_V2);
    out[MAGIC_V2.len()] = ALG_AES_256_GCM;
    out[KEY_ID_OFFSET..KEY_ID_OFFSET + 8].copy_from_slice(&key.key_id.to_le_bytes());
    out[WRITE_SEQ_OFFSET..WRITE_SEQ_OFFSET + 8].copy_from_slice(&write_seq.to_le_bytes());
    out[PLAIN_LEN_OFFSET..PLAIN_LEN_OFFSET + 8]
        .copy_from_slice(&(plain.len() as u64).to_le_bytes());
    out[NONCE_OFFSET..NONCE_OFFSET + NONCE_LEN].copy_from_slice(&nonce_bytes);

    let aad = build_aad(&out[..AAD_HEADER_LEN], domain, rel_path);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let sealed = key
        .cipher()
        .encrypt(
            nonce,
            Payload {
                msg: plain,
                aad: &aad,
            },
        )
        // GCM 加密对任意长度的明文都不会失败（只有 AEAD 的容量上限会），
        // 走到这里说明内存已经坏到不能继续写盘。
        .expect("AES-256-GCM 加密失败（内存异常）");
    let (ciphertext, tag) = sealed.split_at(plain.len());
    out[TAG_OFFSET..TAG_OFFSET + TAG_LEN].copy_from_slice(tag);
    out[CONTAINER_PREFIX_LEN..].copy_from_slice(ciphertext);
    out
}

/// 打开 v2 容器。
///
/// `domain` 与 `rel_path` 必须是**当初封包时用的那一对**：它们进 AAD，对不上就是
/// `Tampered` —— 这正是「密文不能搬家」的实现方式。
pub fn open(
    key: &Key,
    domain: &[u8],
    rel_path: Option<&str>,
    bytes: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    // 先判魔数再判长度：空文件/半截文件报 `Malformed`，而「认得出的旧格式」要报
    // `NotV2`，否则调用方永远不会去走 v1/v0 兼容路径。
    if bytes.len() < MAGIC_V2.len() {
        return Err(CryptoError::Malformed);
    }
    if &bytes[..MAGIC_V2.len()] != MAGIC_V2 {
        return Err(CryptoError::NotV2);
    }
    if bytes.len() < CONTAINER_PREFIX_LEN {
        return Err(CryptoError::Malformed);
    }
    if bytes[MAGIC_V2.len()] != ALG_AES_256_GCM {
        return Err(CryptoError::Malformed);
    }
    // key_id 先比：用错了密钥时「校验失败」和「密钥不对」是两件完全不同的事，
    // 能分清就必须分清，否则用户拿到的诊断永远是「文件损坏」。
    if read_u64(bytes, KEY_ID_OFFSET) != key.key_id {
        return Err(CryptoError::WrongKey);
    }
    let plain_len = read_u64(bytes, PLAIN_LEN_OFFSET);
    let ciphertext = &bytes[CONTAINER_PREFIX_LEN..];
    if plain_len != ciphertext.len() as u64 {
        return Err(CryptoError::Malformed);
    }

    let aad = build_aad(&bytes[..AAD_HEADER_LEN], domain, rel_path);
    // `aes-gcm` 的 `decrypt` 要求「密文 + tag」连在一起，容器里是分开存的，这里拼回去。
    let mut joined = Vec::with_capacity(ciphertext.len() + TAG_LEN);
    joined.extend_from_slice(ciphertext);
    joined.extend_from_slice(&bytes[TAG_OFFSET..TAG_OFFSET + TAG_LEN]);
    let nonce = Nonce::from_slice(&bytes[NONCE_OFFSET..NONCE_OFFSET + NONCE_LEN]);
    let plain = key
        .cipher()
        .decrypt(
            nonce,
            Payload {
                msg: &joined,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Tampered)?;
    if plain.len() as u64 != plain_len {
        // 理论上不可达（tag 覆盖了 plain_len）。留着是因为「解密成功但长度不对」必须
        // 报结构错误而不是把半截明文交出去。
        return Err(CryptoError::Malformed);
    }
    Ok(plain)
}

/// AAD = 容器头前 32 字节 ‖ 域串 ‖ `0x00` ‖ 相对路径（实体才有路径）。
///
/// 分隔符必须是域串里不可能出现的字节：没有它，`("text", Some("a"))` 与
/// `("texta", None)` 会算出同一段 AAD。
fn build_aad(header: &[u8], domain: &[u8], rel_path: Option<&str>) -> Vec<u8> {
    let path_len = rel_path.map_or(0, |p| p.len());
    let mut aad = Vec::with_capacity(AAD_HEADER_LEN + domain.len() + 1 + path_len);
    aad.extend_from_slice(header);
    aad.extend_from_slice(domain);
    aad.push(0);
    if let Some(p) = rel_path {
        aad.extend_from_slice(p.as_bytes());
    }
    aad
}

/// 从固定偏移读一个小端 u64。用显式下标而不是 `try_into().unwrap()`：
/// 这条路径上的 panic 只会发生在文件被截断时，而那种情况必须报错不能崩。
fn read_u64(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
        bytes[off + 4],
        bytes[off + 5],
        bytes[off + 6],
        bytes[off + 7],
    ])
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn random_bytes(buf: &mut [u8]) -> io::Result<()> {
    getrandom::getrandom(buf).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
}

// ---------------------------------------------------------------------------
// DPAPI：只包 32 字节密钥
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod keyring {
    use std::ffi::c_void;

    /// Win32 `DATA_BLOB`
    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            p_data_in: *const DataBlob,
            sz_data_descr: *const u16,
            p_optional_entropy: *const DataBlob,
            pv_reserved: *mut c_void,
            p_prompt_struct: *mut c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;
        fn CryptUnprotectData(
            p_data_in: *const DataBlob,
            ppsz_data_descr: *mut *mut u16,
            p_optional_entropy: *const DataBlob,
            pv_reserved: *mut c_void,
            p_prompt_struct: *mut c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(h: *mut c_void) -> *mut c_void;
    }

    /// 常驻后台程序弹不出任何对话框，弹窗会卡死在无人值守的场景。
    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x01;

    fn blob_of(bytes: &[u8]) -> DataBlob {
        DataBlob {
            cb_data: bytes.len() as u32,
            // DPAPI 只读入参，但 Win32 的签名是非 const 指针
            pb_data: bytes.as_ptr() as *mut u8,
        }
    }

    /// 调一次 DPAPI 并把输出拷成 `Vec`，顺手 `LocalFree`（否则每次调用漏一块内存）。
    fn call(input: &[u8], entropy: &[u8], protect: bool) -> Option<Vec<u8>> {
        unsafe {
            let in_blob = blob_of(input);
            let ent = blob_of(entropy);
            let mut out = DataBlob {
                cb_data: 0,
                pb_data: std::ptr::null_mut(),
            };
            let ok = if protect {
                CryptProtectData(
                    &in_blob,
                    std::ptr::null(),
                    &ent,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            } else {
                CryptUnprotectData(
                    &in_blob,
                    std::ptr::null_mut(),
                    &ent,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
            };
            if ok == 0 || out.pb_data.is_null() {
                return None;
            }
            let v = std::slice::from_raw_parts(out.pb_data, out.cb_data as usize).to_vec();
            LocalFree(out.pb_data as *mut c_void);
            Some(v)
        }
    }

    pub(super) fn protect(input: &[u8], entropy: &[u8]) -> Option<Vec<u8>> {
        call(input, entropy, true)
    }

    pub(super) fn unprotect(input: &[u8], entropy: &[u8]) -> Option<Vec<u8>> {
        call(input, entropy, false)
    }
}

/// 非 Windows 平台没有 DPAPI。项目只发布 Windows 版，这个分支是为了让容器部分
/// （纯 Rust）在别的平台上仍能编译与单测；密钥文件在这条路径上永远创建不出来。
#[cfg(not(windows))]
mod keyring {
    pub(super) fn protect(_input: &[u8], _entropy: &[u8]) -> Option<Vec<u8>> {
        None
    }

    pub(super) fn unprotect(_input: &[u8], _entropy: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 MB 用例的明文长度。取这个值是为了跨过「大对象分配」的量级，
    /// 同时不至于让单测慢到没人愿意跑。
    const BIG_LEN: usize = 5 * 1024 * 1024;

    /// 自建临时目录，`Drop` 时清掉。
    ///
    /// 不用第三方 tempfile：本仓库的纪律是能不引依赖就不引，这里十几行够了。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "qingtie-crypto-{tag}-{}-{}",
                std::process::id(),
                now_ms()
            ));
            fs::create_dir_all(&p).expect("创建临时目录");
            TempDir(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 不经过 DPAPI 直接造一把密钥：容器相关的用例都不该依赖「当前进程能不能调 DPAPI」，
    /// 否则在无桌面会话里跑测试会得到一堆与密码学无关的失败。
    fn test_key(seed: u8) -> Key {
        Key {
            bytes: [seed; KEY_LEN],
            key_id: 0x0102_0304_0506_0708u64 ^ (seed as u64).wrapping_mul(0x9e37_79b9),
            created_ms: 1_700_000_000_000 + seed as u64,
        }
    }

    #[test]
    fn seal_open_roundtrip_index_and_entity() {
        let key = test_key(1);

        // 索引域：空明文也要能往返（空索引是合法状态，不是异常）。
        let sealed = seal(&key, DOMAIN_INDEX, None, b"", 7);
        assert_eq!(sealed.len(), CONTAINER_PREFIX_LEN);
        assert_eq!(
            open(&key, DOMAIN_INDEX, None, &sealed).expect("空明文应能解开"),
            Vec::<u8>::new()
        );

        // 实体域 + 相对路径。
        let text = "轻贴：一段用于往返验证的正文".as_bytes();
        let sealed = seal(&key, DOMAIN_TEXT, Some("data/ab12-1.txt"), text, 0);
        let plain =
            open(&key, DOMAIN_TEXT, Some("data/ab12-1.txt"), &sealed).expect("实体应能解开");
        assert_eq!(plain, text);

        // 5 MB：跨过「一次性大块分配」的路径，确认长度字段与密文长度一致。
        let big = vec![0x5au8; BIG_LEN];
        let sealed = seal(&key, DOMAIN_FILES, Some("files/ab12-1.txt"), &big, 0);
        assert_eq!(sealed.len(), CONTAINER_PREFIX_LEN + BIG_LEN);
        let plain =
            open(&key, DOMAIN_FILES, Some("files/ab12-1.txt"), &sealed).expect("大对象应能解开");
        assert_eq!(plain.len(), BIG_LEN);
        assert!(plain.iter().all(|b| *b == 0x5a));
    }

    #[test]
    fn nonce_is_random_per_seal() {
        let key = test_key(2);
        let plain = b"same plaintext, two seals";
        let a = seal(&key, DOMAIN_INDEX, None, plain, 1);
        let b = seal(&key, DOMAIN_INDEX, None, plain, 1);
        assert_ne!(a, b, "同样明文两次封包必须不同");
        assert_ne!(
            &a[NONCE_OFFSET..NONCE_OFFSET + NONCE_LEN],
            &b[NONCE_OFFSET..NONCE_OFFSET + NONCE_LEN],
            "差异必须来自 nonce"
        );
        assert_ne!(
            &a[CONTAINER_PREFIX_LEN..],
            &b[CONTAINER_PREFIX_LEN..],
            "nonce 变了，CTR 密钥流就变了，密文体也必须不同"
        );
        assert_eq!(open(&key, DOMAIN_INDEX, None, &a).unwrap(), plain.to_vec());
        assert_eq!(open(&key, DOMAIN_INDEX, None, &b).unwrap(), plain.to_vec());
    }

    #[test]
    fn moved_ciphertext_is_rejected() {
        let key = test_key(3);
        let plain = b"a.txt own content";
        let sealed = seal(&key, DOMAIN_TEXT, Some("data/a.txt"), plain, 0);

        // ① 原样搬到另一个路径
        assert_eq!(
            open(&key, DOMAIN_TEXT, Some("data/b.txt"), &sealed),
            Err(CryptoError::Tampered)
        );
        // ② 去掉路径（当成无路径的容器读）
        assert_eq!(
            open(&key, DOMAIN_TEXT, None, &sealed),
            Err(CryptoError::Tampered)
        );
        // ③ 换域：实体密文冒充索引
        assert_eq!(
            open(&key, DOMAIN_INDEX, Some("data/a.txt"), &sealed),
            Err(CryptoError::Tampered)
        );
        // ④ 路径拼接不含糊：域串 + 路径的组合不会被另一种切分撞上
        assert_eq!(
            open(&key, b"textdata/a.txt", None, &sealed),
            Err(CryptoError::Tampered)
        );
    }

    #[test]
    fn flipped_byte_is_tampered_and_truncation_is_malformed() {
        let key = test_key(4);
        let plain = b"0123456789abcdef";
        let sealed = seal(&key, DOMAIN_TEXT, Some("data/c.txt"), plain, 0);

        // 密文区每个位置翻一位都必须失败（抽头、中、尾三处，避免只测到边界）
        for off in [
            CONTAINER_PREFIX_LEN,
            CONTAINER_PREFIX_LEN + 7,
            sealed.len() - 1,
        ] {
            let mut bad = sealed.clone();
            bad[off] ^= 0x01;
            assert_eq!(
                open(&key, DOMAIN_TEXT, Some("data/c.txt"), &bad),
                Err(CryptoError::Tampered),
                "密文偏移 {off} 翻位应被拒"
            );
        }
        // tag 翻位同理
        let mut bad = sealed.clone();
        bad[TAG_OFFSET + 15] ^= 0x80;
        assert_eq!(
            open(&key, DOMAIN_TEXT, Some("data/c.txt"), &bad),
            Err(CryptoError::Tampered)
        );
        // 头里 write_seq 被改也在 AAD 里，同样失败
        let mut bad = sealed.clone();
        bad[WRITE_SEQ_OFFSET] ^= 0xff;
        assert_eq!(
            open(&key, DOMAIN_TEXT, Some("data/c.txt"), &bad),
            Err(CryptoError::Tampered)
        );

        // 截断尾部：长度与 plain_len 不符 → 结构错误（不是 Tampered）
        let short = &sealed[..sealed.len() - 1];
        assert_eq!(
            open(&key, DOMAIN_TEXT, Some("data/c.txt"), short),
            Err(CryptoError::Malformed)
        );
        // 截到前缀中间
        assert_eq!(
            open(&key, DOMAIN_TEXT, Some("data/c.txt"), &sealed[..30]),
            Err(CryptoError::Malformed)
        );
    }

    #[test]
    fn plain_len_mismatch_is_malformed() {
        let key = test_key(5);
        let plain = vec![7u8; 100];
        let sealed = seal(&key, DOMAIN_INDEX, None, &plain, 1);

        let mut bad = sealed.clone();
        bad[PLAIN_LEN_OFFSET..PLAIN_LEN_OFFSET + 8].copy_from_slice(&101u64.to_le_bytes());
        assert_eq!(
            open(&key, DOMAIN_INDEX, None, &bad),
            Err(CryptoError::Malformed)
        );

        let mut bad = sealed.clone();
        bad[PLAIN_LEN_OFFSET..PLAIN_LEN_OFFSET + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            open(&key, DOMAIN_INDEX, None, &bad),
            Err(CryptoError::Malformed)
        );

        // 未知算法编号也算结构错误
        let mut bad = sealed.clone();
        bad[MAGIC_V2.len()] = 0x02;
        assert_eq!(
            open(&key, DOMAIN_INDEX, None, &bad),
            Err(CryptoError::Malformed)
        );
    }

    #[test]
    fn other_key_reports_wrong_key() {
        let mine = test_key(6);
        let other = test_key(7);
        let sealed = seal(&other, DOMAIN_INDEX, None, b"secret", 1);
        assert_eq!(
            open(&mine, DOMAIN_INDEX, None, &sealed),
            Err(CryptoError::WrongKey)
        );

        // key_id 相同但密钥不同：必须落到 Tampered（说明 key_id 只是诊断字段，
        // 真正的完整性由 GCM 保证，不能把「key_id 对得上」当成验证通过）
        let mut twin = test_key(7);
        twin.bytes = [0x99; KEY_LEN];
        assert_eq!(
            open(&twin, DOMAIN_INDEX, None, &sealed),
            Err(CryptoError::Tampered)
        );
    }

    #[test]
    fn legacy_bytes_are_not_v2() {
        let key = test_key(8);

        // v1：整份 DPAPI
        let v1 = b"\x00QTE1\x01\xd0\x8c\x9d\xdf".to_vec();
        assert_eq!(open(&key, DOMAIN_INDEX, None, &v1), Err(CryptoError::NotV2));
        assert_eq!(detect(&v1), Format::V1Dpapi);

        // v0：明文 JSON
        for raw in [b"{\"a\":1}".to_vec(), b"  [1,2]".to_vec()] {
            assert_eq!(
                open(&key, DOMAIN_INDEX, None, &raw),
                Err(CryptoError::NotV2)
            );
            assert_eq!(detect(&raw), Format::V0Plaintext);
        }

        // 空文件与认不出来的东西：不是旧格式，是损坏
        assert_eq!(
            open(&key, DOMAIN_INDEX, None, b""),
            Err(CryptoError::Malformed)
        );
        assert_eq!(detect(b""), Format::Unknown);
        assert_eq!(detect(b"\x00QTE2\x01"), Format::V2);
        assert_eq!(detect(b"hello"), Format::Unknown);

        // v2 魔数但被截在头里：结构错误
        assert_eq!(
            open(&key, DOMAIN_INDEX, None, b"\x00QTE2\x01\x01"),
            Err(CryptoError::Malformed)
        );
    }

    #[test]
    fn key_file_roundtrip_and_corruption() {
        let dir = TempDir::new("key");
        let path = key_path(dir.path());
        assert!(!path.exists(), "初始不应有密钥文件");

        let first = load_or_create_key(dir.path()).expect("首次应生成密钥");
        assert!(path.exists(), "密钥文件应落盘");
        let raw = fs::read(&path).expect("读密钥文件");
        assert!(raw.len() > KEY_HEADER_LEN, "应含 DPAPI blob");
        assert_eq!(&raw[..KEY_MAGIC.len()], KEY_MAGIC);
        assert_eq!(raw[KEY_MAGIC.len()], 0, "保留位应为 0");

        // 读回来的必须是同一把钥匙：key_id 相同，且能用它解开「上一把」封的容器。
        let sealed = seal(&first, DOMAIN_INDEX, None, b"persisted", 1);
        let second = load_or_create_key(dir.path()).expect("已存在应直接读");
        assert_eq!(second.key_id(), first.key_id());
        assert_eq!(second.created_ms(), first.created_ms());
        assert_eq!(
            open(&second, DOMAIN_INDEX, None, &sealed).unwrap(),
            b"persisted"
        );

        // 逐字节改动 DPAPI blob：每一次都必须**要么报错、要么解出同一把密钥**。
        //
        // 为什么不是「随便改一字节就必须报错」：实测 `CryptProtectData` 的 blob 头部
        // （version / provider GUID 等）不在它自己的签名覆盖范围内，改那些字节既不报错
        // 也不影响解出的内容 —— 真正的载荷与签名区才是被校验的。所以这里断言的是更强的
        // 那条性质：不存在「静默解出一把不同的密钥」的偏移。`key_id` 之后的明文头
        // （[8..24)）不参与本次遍历，它在设计上就是明文诊断字段，改它只会让所有容器
        // 变成 `WrongKey`，是可见的失败而不是静默错误。
        let mut rejected = 0usize;
        for off in KEY_HEADER_LEN..raw.len() {
            let mut broken = raw.clone();
            broken[off] ^= 0x01;
            fs::write(&path, &broken).expect("写坏文件");
            match load_or_create_key(dir.path()) {
                Ok(loaded) => assert_eq!(
                    loaded.bytes, first.bytes,
                    "偏移 {off} 改动后解出的密钥与原密钥不一致"
                ),
                Err(e) => {
                    assert!(
                        e.to_string().contains("不会自动重建"),
                        "偏移 {off} 的错误文案要说清不会重建：{e}"
                    );
                    rejected += 1;
                }
            }
            assert_eq!(
                fs::read(&path).expect("坏文件应保持原样"),
                broken,
                "偏移 {off}：失败时不许覆盖用户的密钥文件"
            );
        }
        assert!(
            rejected > 0,
            "签名区/载荷区被改动必须被 DPAPI 判成损坏，实际一次都没拒绝"
        );

        // 恢复原文件，继续测结构类损坏。
        fs::write(&path, &raw).expect("恢复密钥文件");
        assert_eq!(
            load_or_create_key(dir.path())
                .expect("恢复后应能读回")
                .key_id(),
            first.key_id()
        );

        // 截断到头部之前
        fs::write(&path, &raw[..10]).expect("写截断文件");
        let err = load_or_create_key(dir.path()).expect_err("截断文件必须报错");
        assert!(err.to_string().contains("长度不足"), "截断应报长度：{err}");

        // 魔数被改
        let mut wrong_magic = raw.clone();
        wrong_magic[1] = b'X';
        fs::write(&path, &wrong_magic).expect("写魔数错误文件");
        let err = load_or_create_key(dir.path()).expect_err("魔数不符必须报错");
        assert!(err.to_string().contains("魔数"), "应报魔数：{err}");
    }

    #[test]
    fn key_debug_never_leaks_bytes() {
        let key = test_key(9);
        let text = format!("{key:?}");
        assert!(text.contains("***"), "Debug 必须遮蔽密钥：{text}");
        assert!(!text.contains("09090909"), "Debug 不能出现密钥字节：{text}");
    }
}
