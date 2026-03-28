/// Parser for /etc/passwd files.
///
/// Format: username:uid:gid:home:shell (one entry per line)

pub struct PasswdEntry {
    pub username: [u8; 32],
    pub username_len: usize,
    pub uid: u32,
    pub gid: u32,
    pub home: [u8; 64],
    pub home_len: usize,
    pub shell: [u8; 64],
    pub shell_len: usize,
}

impl PasswdEntry {
    pub fn username(&self) -> &[u8] {
        &self.username[..self.username_len]
    }

    pub fn home(&self) -> &[u8] {
        &self.home[..self.home_len]
    }

    pub fn shell(&self) -> &[u8] {
        &self.shell[..self.shell_len]
    }
}

/// Look up a user by name in passwd file data.
pub fn lookup_user(data: &[u8], username: &[u8]) -> Option<PasswdEntry> {
    let mut pos = 0;
    while pos < data.len() {
        // Find end of line
        let line_end = data[pos..].iter().position(|&b| b == b'\n')
            .map_or(data.len(), |p| pos + p);
        let line = &data[pos..line_end];
        pos = line_end + 1;

        if line.is_empty() {
            continue;
        }

        if let Some(entry) = parse_line(line) {
            if entry.username_len == username.len()
                && entry.username[..entry.username_len] == *username
            {
                return Some(entry);
            }
        }
    }
    None
}

fn parse_line(line: &[u8]) -> Option<PasswdEntry> {
    let mut fields = [&[][..]; 5];
    let mut field_count = 0;
    let mut start = 0;

    for i in 0..line.len() {
        if line[i] == b':' {
            if field_count < 5 {
                fields[field_count] = &line[start..i];
                field_count += 1;
            }
            start = i + 1;
        }
    }
    // Last field (no trailing colon)
    if field_count < 5 {
        fields[field_count] = &line[start..];
        field_count += 1;
    }

    if field_count < 5 {
        return None;
    }

    let uid = parse_u32(fields[1])?;
    let gid = parse_u32(fields[2])?;

    let mut entry = PasswdEntry {
        username: [0; 32],
        username_len: 0,
        uid,
        gid,
        home: [0; 64],
        home_len: 0,
        shell: [0; 64],
        shell_len: 0,
    };

    let ulen = fields[0].len().min(32);
    entry.username[..ulen].copy_from_slice(&fields[0][..ulen]);
    entry.username_len = ulen;

    let hlen = fields[3].len().min(64);
    entry.home[..hlen].copy_from_slice(&fields[3][..hlen]);
    entry.home_len = hlen;

    let slen = fields[4].len().min(64);
    entry.shell[..slen].copy_from_slice(&fields[4][..slen]);
    entry.shell_len = slen;

    Some(entry)
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut val: u32 = 0;
    for &b in s {
        if b < b'0' || b > b'9' {
            return None;
        }
        val = val.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(val)
}
