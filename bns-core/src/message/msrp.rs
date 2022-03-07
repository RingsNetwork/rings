use crate::message::Did;
use anyhow::anyhow;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

pub enum Msrp {
    MsrpSend(MsrpSend),
    MsrpReport(MsrpReport),
}

#[derive(Debug, PartialEq, Clone, Deserialize, Serialize)]
pub struct MsrpSend {
    pub transaction_id: String,
    pub from_path: Vec<Did>,
}

#[derive(Debug, PartialEq, Clone, Deserialize, Serialize)]
pub struct MsrpReport {
    pub transaction_id: String,
    pub from_path: Vec<Did>,
    pub to_path: Vec<Did>,
}

impl From<MsrpSend> for MsrpReport {
    fn from(send: MsrpSend) -> Self {
        Self {
            transaction_id: send.transaction_id,
            from_path: Vec::new(),
            to_path: send.from_path,
        }
    }
}

// TODO: How to prevent malicious path tampering?

impl MsrpSend {
    #[must_use]
    pub fn record(&self, prev: &Did) -> Self {
        // A -> B -> C
        // C will push B into from_path, which should be [A], **after** got message from B.
        // Then from_path become [A, B].

        let mut c = self.clone();
        c.from_path.push(*prev);
        c
    }
}

impl MsrpReport {
    pub fn record(&self, prev: &Did, current: &Did) -> Result<Self> {
        // A <- B <- C
        // B will check and pop B from to_path, which should be [A, B], **after** got message from C.
        // Then B will push C into from_path, which should be [].
        // Then from_path become [C], and to_path become [A].

        if self.to_path.last() != Some(current) {
            return Err(anyhow!("Invalid path"));
        }

        let mut c = self.clone();
        c.to_path.pop();
        c.from_path.push(*prev);
        Ok(c)
    }
}
