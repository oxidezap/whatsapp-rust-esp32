use std::error::Error;
use std::io::ErrorKind;

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
}
