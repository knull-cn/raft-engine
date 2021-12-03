

pub enum Error {
    InvalidArgument(String),

    Corruption(String),
}

impl Error{
    pub fn to_string(&self) ->String {
        return match self { 
            Error::InvalidArgument(s1) => s1.to_string(),
            Error::Corruption(s2) => s2.to_string(),
        };
    }
}