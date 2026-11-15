use super::*;

pub fn basic_authorization(credentials: &Credentials) -> String {
    let value = STANDARD.encode(format!("{}:{}", credentials.username, credentials.password));
    format!("Basic {value}")
}
