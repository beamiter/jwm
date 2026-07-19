// src/backend/error.rs
use std::borrow::Cow;
use std::{error::Error, fmt};

/// 后端错误发生的架构边界（display / device / renderer / IPC）。
///
/// 这些值与 `docs/roadmap.md` Phase 1 中列出的支持边界一一对应，用于把
/// 一个底层失败归类到用户可以理解的启动或运行阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorBoundary {
    /// Display-server connection, window-manager selection and setup.
    Display,
    /// Input, DRM/KMS, session and other hardware device access.
    Device,
    /// GL/EGL/GLX renderer and compositor resource management.
    Renderer,
    /// The private control socket and IPC protocol surface.
    Ipc,
}

impl ErrorBoundary {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Device => "device",
            Self::Renderer => "renderer",
            Self::Ipc => "ipc",
        }
    }
}

impl fmt::Display for ErrorBoundary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 附着在 `BackendError` 上的后端标记上下文。
///
/// `backend` 是产生错误的具体后端名（如 `x11rb`、`wayland-udev`），
/// `operation` 用一句话描述失败时正在进行的操作。上下文只增加信息，
/// 原始错误始终通过 `source()` 链保留。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendErrorContext {
    pub backend: Cow<'static, str>,
    pub boundary: ErrorBoundary,
    pub operation: Cow<'static, str>,
}

impl BackendErrorContext {
    pub fn new(
        backend: impl Into<Cow<'static, str>>,
        boundary: ErrorBoundary,
        operation: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            backend: backend.into(),
            boundary,
            operation: operation.into(),
        }
    }
}

impl fmt::Display for BackendErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}/{}] {}", self.backend, self.boundary, self.operation)
    }
}

#[derive(Debug)]
pub enum BackendError {
    Unsupported(&'static str),
    NotFound(&'static str),
    Message(String),
    Other(Box<dyn Error + Send + Sync>),
    /// 带有后端标记上下文的错误；原始错误保存在 `source` 中。
    Contextual {
        context: BackendErrorContext,
        source: Box<BackendError>,
    },
}

impl BackendError {
    /// 给错误附加一层后端标记上下文。
    #[must_use]
    pub fn with_context(self, context: BackendErrorContext) -> Self {
        Self::Contextual {
            context,
            source: Box::new(self),
        }
    }

    /// 最外层的后端标记上下文（若有）。
    #[must_use]
    pub fn context(&self) -> Option<&BackendErrorContext> {
        match self {
            Self::Contextual { context, .. } => Some(context),
            _ => None,
        }
    }

    /// 剥离所有上下文层后的原始错误，便于按错误种类匹配。
    #[must_use]
    pub fn root_cause(&self) -> &BackendError {
        match self {
            Self::Contextual { source, .. } => source.root_cause(),
            other => other,
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Unsupported(s) => write!(f, "Unsupported: {s}"),
            BackendError::NotFound(s) => write!(f, "Not found: {s}"),
            BackendError::Message(s) => write!(f, "Error: {s}"),
            BackendError::Other(e) => write!(f, "{e}"),
            BackendError::Contextual { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl Error for BackendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            BackendError::Other(e) => Some(&**e),
            BackendError::Contextual { source, .. } => Some(&**source),
            _ => None,
        }
    }
}

/// 在平台边界为 `Result` 附加后端标记上下文的扩展方法。
pub trait BackendContextExt<T> {
    /// 把错误转换为 `BackendError` 并附加 `[backend/boundary] operation` 上下文。
    ///
    /// # Errors
    ///
    /// 原始错误 `E` 会原样转换为 `BackendError` 并包上一层上下文返回。
    fn backend_context(
        self,
        backend: impl Into<Cow<'static, str>>,
        boundary: ErrorBoundary,
        operation: impl Into<Cow<'static, str>>,
    ) -> Result<T, BackendError>;
}

impl<T, E: Into<BackendError>> BackendContextExt<T> for Result<T, E> {
    fn backend_context(
        self,
        backend: impl Into<Cow<'static, str>>,
        boundary: ErrorBoundary,
        operation: impl Into<Cow<'static, str>>,
    ) -> Result<T, BackendError> {
        self.map_err(|error| {
            error
                .into()
                .with_context(BackendErrorContext::new(backend, boundary, operation))
        })
    }
}

// === 标准库与基础类型 ===

impl From<std::io::Error> for BackendError {
    fn from(e: std::io::Error) -> Self {
        BackendError::Other(Box::new(e))
    }
}

impl From<String> for BackendError {
    fn from(s: String) -> Self {
        BackendError::Message(s)
    }
}

impl From<&'static str> for BackendError {
    fn from(s: &'static str) -> Self {
        BackendError::Message(s.to_string())
    }
}

impl From<Box<dyn Error + Send + Sync>> for BackendError {
    fn from(e: Box<dyn Error + Send + Sync>) -> Self {
        BackendError::Other(e)
    }
}

// === Calloop ===

impl From<calloop::Error> for BackendError {
    fn from(e: calloop::Error) -> Self {
        BackendError::Other(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contextual_display_tags_backend_boundary_and_operation() {
        let error = BackendError::from("connection refused".to_string()).with_context(
            BackendErrorContext::new("x11rb", ErrorBoundary::Display, "connect to X server"),
        );

        assert_eq!(
            error.to_string(),
            "[x11rb/display] connect to X server: Error: connection refused"
        );
    }

    #[test]
    fn context_layers_preserve_the_original_error_via_source_and_root_cause() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "socket denied");
        let error = BackendError::from(io).with_context(BackendErrorContext::new(
            "wayland-udev",
            ErrorBoundary::Ipc,
            "bind control socket",
        ));

        let context = error.context().expect("outermost context");
        assert_eq!(context.backend, "wayland-udev");
        assert_eq!(context.boundary, ErrorBoundary::Ipc);

        // source() 链剥离上下文后必须到达原始 IO 错误。
        let inner = error.source().expect("contextual source");
        assert!(inner.to_string().contains("socket denied"));
        assert!(matches!(error.root_cause(), BackendError::Other(_)));
    }

    #[test]
    fn result_extension_converts_and_tags_in_one_step() {
        let result: Result<(), std::io::Error> = Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no DRM node",
        ));
        let error = result
            .backend_context("wayland-udev", ErrorBoundary::Device, "open DRM device")
            .unwrap_err();

        assert_eq!(
            error.context().map(|context| context.boundary),
            Some(ErrorBoundary::Device)
        );
        assert!(error.to_string().starts_with("[wayland-udev/device]"));

        let ok: Result<u32, String> = Ok(7);
        assert_eq!(
            ok.backend_context("x11rb", ErrorBoundary::Renderer, "unused")
                .unwrap(),
            7
        );
    }

    #[test]
    fn boundaries_have_stable_lowercase_names() {
        assert_eq!(ErrorBoundary::Display.as_str(), "display");
        assert_eq!(ErrorBoundary::Device.as_str(), "device");
        assert_eq!(ErrorBoundary::Renderer.as_str(), "renderer");
        assert_eq!(ErrorBoundary::Ipc.as_str(), "ipc");
    }
}
