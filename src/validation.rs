use crate::error::AppError;

pub fn snap_name(value: &str) -> Result<(), AppError> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 40
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && bytes.iter().any(u8::is_ascii_lowercase)
        && !value.ends_with('-')
        && !value.contains("--");
    if valid {
        Ok(())
    } else {
        Err(AppError::BadRequest("invalid snap name".to_owned()))
    }
}

pub fn config_key(value: &str) -> Result<(), AppError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('-')
        && value.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 64
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        });
    if valid {
        Ok(())
    } else {
        Err(AppError::BadRequest("invalid configuration key".to_owned()))
    }
}

pub fn service_name(value: &str) -> Result<(), AppError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(AppError::BadRequest("invalid service name".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{config_key, service_name, snap_name};

    #[test]
    fn validates_snap_names() {
        for valid in ["hello", "hello-world", "a1"] {
            assert!(snap_name(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "-hello", "hello-", "hello--world", "123", "Hello"] {
            assert!(snap_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn validates_keys_and_services() {
        assert!(config_key("ports.http-1").is_ok());
        assert!(config_key("--help").is_err());
        assert!(config_key("ports..http").is_err());
        assert!(service_name("web-worker").is_ok());
        assert!(service_name("../unit").is_err());
    }
}
