use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("文件操作失败 {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("数据损坏或字段类型无效: {0}")]
    InvalidData(PathBuf),
    #[error("不支持的数据版本: {0}")]
    Schema(u32),
    #[error("不安全的路径: {0}")]
    UnsafePath(PathBuf),
    #[error("等待文件锁超时: {0}")]
    LockTimeout(PathBuf),
    #[error("无效的账户名称: {0}")]
    InvalidProfile(String),
    #[error("找不到账户: {0}")]
    AccountMissing(String),
    #[error("账户已存在: {0}")]
    AccountExists(String),
    #[error("账户或会话存在歧义: {0}")]
    Ambiguous(String),
    #[error("账户仍有本地会话或受管恢复计划")]
    AccountInUse,
    #[error("凭据存储不可用；系统凭据库可能未解锁或不可访问")]
    CredentialUnavailable,
    #[error("等待系统凭据库超时（{0} 秒）；请解锁或允许访问，必要时增加 --keyring-timeout")]
    CredentialTimeout(u64),
    #[error("系统凭据库不可用: {0}")]
    CredentialBackend(&'static str),
    #[error("需要登录或重新授权")]
    NeedsAuth,
    #[error("OAuth 配置或请求无效: {0}")]
    OAuth(&'static str),
    #[error("OAuth 服务请求失败 (HTTP {0})")]
    OAuthStatus(u16),
    #[error("OAuth 网络请求失败")]
    OAuthTransport,
    #[error("不支持的 ADC 类型: {0}")]
    UnsupportedAdc(String),
    #[error("控制面请求失败 (HTTP {0})")]
    ControlStatus(u16),
    #[error("控制面网络请求失败；远端操作结果可能未知")]
    ControlTransport { retryable: bool },
    #[error("控制面协议或参数无效: {0}")]
    ControlProtocol(&'static str),
    #[error("已有待恢复控制面操作；请用相同参数重试 new 或 stop")]
    ControlPending,
    #[error("运行时网络请求失败（请求结果可能未知）")]
    RuntimeTransport,
    #[error("运行时请求失败 (HTTP {0})")]
    RuntimeStatus(u16),
    #[error("Contents 协议或参数无效: {0}")]
    ContentsProtocol(&'static str),
    #[error("远端文件或目录不存在")]
    RemoteMissing,
    #[error("不能下载目录")]
    RemoteDirectory,
    #[error("文件或响应超过大小上限 {0} 字节")]
    FileTooLarge(u64),
    #[error("编辑结果未上传，已保留本地文件 {path}: {source}")]
    EditPending { path: PathBuf, source: Box<Error> },
    #[error("编辑器未成功退出；没有上传更改")]
    EditorFailed,
    #[error("账户汇总工作线程失败")]
    InventoryWorker,
    #[error("Jupyter 协议或参数无效: {0}")]
    KernelProtocol(&'static str),
    #[error("WebSocket 建立或传输失败")]
    WebSocketTransport,
    #[error("上次 kernel 重启结果未知；核查后显式重试 restart-kernel")]
    KernelRestartPending,
    #[error("上次 kernel 创建结果未知；请显式指定已有 kernel ID 后恢复，不能自动重复创建")]
    KernelStartPending,
    #[error("远端执行失败")]
    ExecutionFailed,
    #[error("执行结果未知，消息 ID {0}；没有自动重放或取消远端代码")]
    ExecutionUnknown(String),
    #[error("上次执行尚未确认结束，消息 ID {0}；先核查或 restart-kernel")]
    ExecutionPending(String),
    #[error("历史记录操作失败: {0}")]
    History(&'static str),
    #[error("操作 {operation} 已完成，但历史记录失败: {source}")]
    HistoryAfterOperation {
        operation: String,
        source: Box<Error>,
    },
    #[error("run 操作失败: {0}")]
    Run(&'static str),
    #[error("脚本退出码 {0}")]
    ScriptExit(i32),
    #[error("会话不存在: {0}")]
    SessionMissing(String),
    #[error("会话名称已存在: {0}")]
    SessionExists(String),
    #[error("迁移未完成，原始凭据或事务日志已保留")]
    MigrationPending,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}
