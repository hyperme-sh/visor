use super::*;
use std::io::IsTerminal;

pub(super) fn attach(
    connection: &Connection<'_>,
    scope: &Value,
    id: Uuid,
    cursor: u64,
    take_over: bool,
) -> Result<bool, String> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("Attach input from a local terminal".into());
    }
    let mut read = action(scope, "read", id);
    read["cursor"] = json!(cursor);
    let page = connection.exchange(read)?;
    let terminal = &page["terminal"];
    if terminal["input_available"] != true {
        return Err("This task does not have available interactive input".into());
    }
    let previous = &terminal["owner"];
    if !previous.is_null() && !take_over {
        return Err("Terminal input is owned. Use --take-over to explicitly transfer it.".into());
    }
    let terminal_id = terminal["id"].clone();
    let owner = Uuid::new_v4();
    let mut control = action(scope, "control", id);
    control["terminal"] = terminal_id.clone();
    let request = |operation| {
        let mut envelope = control.clone();
        envelope["request"] = operation;
        connection.exchange(envelope)
    };
    let claim = request(
        json!({"type":"claim","owner":owner,"client":"cli","expected_owner":previous["id"]}),
    )?;
    let lease = claim["lease"].clone();
    if lease
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .is_none()
    {
        return Err("Terminal input claim could not be confirmed".into());
    }
    let release = || request(json!({"type":"release","lease":lease}));
    let mode = match TerminalMode::enter() {
        Ok(mode) => mode,
        Err(error) => {
            let _ = release();
            return Err(error);
        }
    };
    let mut input = Input {
        request,
        owner,
        terminal_id,
        lease: lease.clone(),
        sequence: 0,
        dimensions: None,
    };
    eprintln!("Input attached to task {id}. Ctrl+] detaches.");
    let outcome = watch(connection, scope, id, cursor, Some(&mut input));
    drop(mode);
    let _ = release();
    eprintln!(
        "Detached from task {id} after acknowledgement {}.",
        input.sequence
    );
    outcome
}

pub(super) trait Attachment {
    fn advance(&mut self, page: &Value) -> Result<bool, String>;
}

struct Input<F> {
    request: F,
    owner: Uuid,
    terminal_id: Value,
    lease: Value,
    sequence: u64,
    dimensions: Option<(u16, u16)>,
}

impl<F: Fn(Value) -> Result<Value, String>> Input<F> {
    fn send(&mut self, mut operation: Value, expected_bytes: Option<usize>) -> Result<(), String> {
        let sequence = self.sequence + 1;
        operation["sequence"] = json!(sequence);
        operation["lease"] = self.lease.clone();
        let result = (self.request)(operation).map_err(|_| {
            "Terminal operation was not confirmed. Attach again before sending more input."
        })?;
        let ack = &result["ack"];
        if ack["sequence"] != sequence || ack["resized"] != expected_bytes.is_none() {
            return Err("Invalid terminal acknowledgement".into());
        }
        self.sequence = sequence;
        if let Some(count) = expected_bytes {
            if ack["accepted_bytes"] != count {
                return Err(format!(
                    "Terminal accepted {} of {count} bytes.",
                    ack["accepted_bytes"]
                ));
            }
        }
        Ok(())
    }
}

impl<F: Fn(Value) -> Result<Value, String>> Attachment for Input<F> {
    fn advance(&mut self, page: &Value) -> Result<bool, String> {
        let terminal = &page["terminal"];
        if terminal["id"] != self.terminal_id
            || terminal["input_available"] != true
            || terminal["owner"]["id"] != json!(self.owner)
        {
            return Err("Terminal input access changed.".into());
        }
        if let Some((rows, cols)) = dimensions() {
            if self.dimensions != Some((rows, cols)) {
                self.send(json!({"type":"resize","rows":rows,"cols":cols}), None)?;
                self.dimensions = Some((rows, cols));
            }
        }
        let mut buffer = [0u8; 1024];
        let length = std::io::stdin()
            .read(&mut buffer)
            .map_err(|_| "Cannot read local terminal input")?;
        let bytes = &buffer[..length];
        let detach = bytes.iter().position(|byte| *byte == 0x1d);
        let forward = &bytes[..detach.unwrap_or(bytes.len())];
        if !forward.is_empty() {
            self.send(
                json!({"type":"input","data_base64":STANDARD.encode(forward)}),
                Some(forward.len()),
            )?;
        }
        Ok(detach.is_none())
    }
}

fn dimensions() -> Option<(u16, u16)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) } == 0
        && (1..=1000).contains(&size.ws_row)
        && (1..=1000).contains(&size.ws_col)
    {
        Some((size.ws_row, size.ws_col))
    } else {
        None
    }
}

struct TerminalMode(libc::termios);
impl TerminalMode {
    fn enter() -> Result<Self, String> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
            return Err("Cannot read local terminal mode".into());
        }
        let mut active = saved;
        active.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        active.c_iflag &= !(libc::ICRNL | libc::INLCR | libc::IGNCR | libc::IXON);
        active.c_cc[libc::VMIN] = 0;
        active.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(0, libc::TCSAFLUSH, &active) } != 0 {
            return Err("Cannot set local terminal mode".into());
        }
        Ok(Self(saved))
    }
}

impl Drop for TerminalMode {
    fn drop(&mut self) {
        // Flush keystrokes that were still local when the attachment ended.
        unsafe {
            libc::tcsetattr(0, libc::TCSAFLUSH, &self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn acknowledgement_advances_once_even_when_only_part_of_input_was_accepted() {
        let sent = RefCell::new(Vec::new());
        let request = |operation: Value| {
            sent.borrow_mut().push(operation.clone());
            Ok(json!({"ack":{"sequence":operation["sequence"],"resized":false,"accepted_bytes":1}}))
        };
        let token = json!(Uuid::new_v4());
        let mut input = Input {
            request,
            owner: Uuid::new_v4(),
            terminal_id: json!(Uuid::new_v4()),
            lease: token.clone(),
            sequence: 0,
            dimensions: None,
        };
        assert!(input
            .send(json!({"type":"input","data_base64":"YWI="}), Some(2))
            .is_err());
        assert_eq!(input.sequence, 1);
        assert_eq!(
            sent.into_inner(),
            vec![json!({"type":"input","data_base64":"YWI=","lease":token,"sequence":1})]
        );
    }
}
