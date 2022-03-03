use crate::did::Did;

#[derive(Debug)]
pub struct Routing {
    pub current: Did,
    pub successor: Did,
    pub predecessor: Option<Did>,
    pub fix_finger_index: u8,
    finger_tables: Vec<Did>,
}

impl Routing {
    pub fn new(current: Did) -> Self {
        return Self {
            current,
            predecessor: None,
            successor: current,
            finger_tables: vec![],
            fix_finger_index: 0,
        };
    }
}
