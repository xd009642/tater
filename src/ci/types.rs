use serde::Deserialize;
use std::borrow::Cow;

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum SingleOrMultiString {
    Single(String),
    Multi(Vec<String>),
}

impl SingleOrMultiString {
    pub fn lines<'a>(&'a self) -> Box<dyn Iterator<Item = &'a str> + 'a> {
        match self {
            Self::Single(ref s) => Box::new(s.lines()),
            // TODO this assumes that each String is single lined
            Self::Multi(ref s) => Box::new(s.iter().map(|x| x.as_str())),
        }
    }

    pub fn joined<'a>(&'a self) -> Cow<'a, str> {
        match self {
            Self::Single(ref s) => Cow::Borrowed(s.as_str()),
            Self::Multi(ref s) => Cow::Owned(s.join("\n")),
        }
    }
}
