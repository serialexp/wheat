//! The repository a connection is for, named by its WebSocket path: `/owner/repo`,
//! each segment percent-encoded. The client builds the path from the repository name
//! and the server reads the name back from it, so a front door can route a connection
//! by its upgrade request alone. Both refuse the same spellings: an empty, `.` or `..`
//! segment, an encoded `/`, `\` or NUL, and a name that is not UTF-8.

/// A path that does not name a repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryPathError {
    /// The path does not start with `/`.
    NotAbsolute,
    /// An encoded `/`, `\` or NUL, which could move a segment boundary.
    EncodedSeparator,
    /// A `%` not followed by two hexadecimal digits.
    MalformedEscape,
    /// A segment that decodes to bytes that are not UTF-8.
    NotUtf8,
    /// An empty, `.` or `..` segment, or one holding a separator or NUL.
    InvalidSegment,
}

impl std::fmt::Display for RepositoryPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RepositoryPathError::NotAbsolute => "repository path is not absolute",
            RepositoryPathError::EncodedSeparator => {
                "encoded path separators or NUL are not allowed"
            }
            RepositoryPathError::MalformedEscape => "malformed percent escape in repository path",
            RepositoryPathError::NotUtf8 => "repository path is not UTF-8",
            RepositoryPathError::InvalidSegment => {
                "empty, dot or separator repository path segment"
            }
        })
    }
}

impl std::error::Error for RepositoryPathError {}

/// The path naming repository `name`: each segment percent-encoded except for
/// unreserved characters.
pub fn repository_path(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut path = String::with_capacity(name.len() + 1);
    for segment in name.split('/') {
        path.push('/');
        for byte in segment.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                path.push(char::from(byte));
            } else {
                path.push('%');
                path.push(char::from(HEX[(byte >> 4) as usize]));
                path.push(char::from(HEX[(byte & 0x0f) as usize]));
            }
        }
    }
    path
}

/// The repository `path` names.
pub fn repository_name(path: &str) -> Result<String, RepositoryPathError> {
    // Checked on the escaped path, before decoding could hide a separator.
    if has_encoded_separator(path) {
        return Err(RepositoryPathError::EncodedSeparator);
    }
    let path = path
        .strip_prefix('/')
        .ok_or(RepositoryPathError::NotAbsolute)?;
    let mut name = String::with_capacity(path.len());
    for (index, segment) in path.split('/').enumerate() {
        let decoded = percent_decode(segment)?;
        if decoded.is_empty()
            || decoded == "."
            || decoded == ".."
            || decoded.contains(['/', '\\', '\0'])
        {
            return Err(RepositoryPathError::InvalidSegment);
        }
        if index > 0 {
            name.push('/');
        }
        name.push_str(&decoded);
    }
    Ok(name)
}

fn has_encoded_separator(path: &str) -> bool {
    path.as_bytes().windows(3).any(|bytes| {
        bytes[0] == b'%'
            && matches!(
                (bytes[1].to_ascii_lowercase(), bytes[2].to_ascii_lowercase()),
                (b'2', b'f') | (b'5', b'c') | (b'0', b'0')
            )
    })
}

fn percent_decode(segment: &str) -> Result<String, RepositoryPathError> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let digit = |offset: usize| {
                bytes
                    .get(index + offset)
                    .and_then(|&byte| (byte as char).to_digit(16))
                    .ok_or(RepositoryPathError::MalformedEscape)
            };
            decoded.push((digit(1)? << 4 | digit(2)?) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| RepositoryPathError::NotUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_survives_its_path() {
        for name in ["owner/repo", "Owner/Répo", "a b/c%d", "one", "x/y/z"] {
            assert_eq!(repository_name(&repository_path(name)).unwrap(), name);
        }
        assert_eq!(repository_path("Owner/Répo"), "/Owner/R%C3%A9po");
    }

    #[test]
    fn ambiguous_paths_are_refused() {
        for (path, error) in [
            ("owner/repo", RepositoryPathError::NotAbsolute),
            ("/", RepositoryPathError::InvalidSegment),
            ("/owner//repo", RepositoryPathError::InvalidSegment),
            ("/owner/./repo", RepositoryPathError::InvalidSegment),
            ("/owner/../repo", RepositoryPathError::InvalidSegment),
            ("/owner/", RepositoryPathError::InvalidSegment),
            ("/owner%2Frepo", RepositoryPathError::EncodedSeparator),
            ("/owner%5crepo", RepositoryPathError::EncodedSeparator),
            ("/owner/%00repo", RepositoryPathError::EncodedSeparator),
            ("/owner/re%po", RepositoryPathError::MalformedEscape),
            ("/owner/repo%4", RepositoryPathError::MalformedEscape),
            ("/owner/%FF", RepositoryPathError::NotUtf8),
            ("/owner/a\\b", RepositoryPathError::InvalidSegment),
        ] {
            assert_eq!(repository_name(path), Err(error), "{path}");
        }
    }
}
