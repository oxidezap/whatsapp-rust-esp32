use std::error::Error;
use std::io::ErrorKind;

#[derive(Debug)]
struct Cause(anyhow::Error);

impl std::fmt::Display for Cause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, formatter)
    }
}

impl Error for Cause {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

pub(crate) fn box_cause(error: anyhow::Error) -> Box<dyn Error + Send + Sync> {
    Box::new(Cause(error))
}

pub(crate) fn tls_connect_error(ret: i32, timeout: bool) -> anyhow::Error {
    let message = format!("TLS connection failed (ret={ret})");
    if timeout {
        std::io::Error::new(ErrorKind::TimedOut, message).into()
    } else {
        anyhow::anyhow!(message)
    }
}

pub(super) fn is_timeout(mut error: &(dyn Error + 'static)) -> bool {
    loop {
        if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
        }) {
            return true;
        }
        match error.source() {
            Some(source) => error = source,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Wrapper(std::io::Error);

    impl std::fmt::Display for Wrapper {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "request failed: {}", self.0)
        }
    }

    impl Error for Wrapper {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn recognizes_timeouts_through_error_sources() {
        for kind in [ErrorKind::TimedOut, ErrorKind::WouldBlock] {
            let error = Wrapper(std::io::Error::from(kind));
            assert!(is_timeout(&error));
        }
        let error = Wrapper(std::io::Error::from(ErrorKind::ConnectionReset));
        assert!(!is_timeout(&error));
    }

    #[test]
    fn boxed_error_keeps_context_and_concrete_cause() {
        let original = std::io::Error::from(ErrorKind::ConnectionReset);
        for error in [
            anyhow::Error::new(original),
            anyhow::Error::new(std::io::Error::from(ErrorKind::ConnectionReset))
                .context("request failed"),
        ] {
            let boxed = box_cause(error);
            let mut source: Option<&(dyn Error + 'static)> = Some(boxed.as_ref());
            let mut found = false;
            while let Some(error) = source {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == ErrorKind::ConnectionReset)
                {
                    found = true;
                    break;
                }
                source = error.source();
            }
            assert!(found);
        }
    }

    #[test]
    fn tls_timeout_is_distinct_from_other_connection_failures() {
        assert!(is_timeout(tls_connect_error(-1, true).as_ref()));
        assert!(!is_timeout(tls_connect_error(-1, false).as_ref()));
    }
}
