#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}

mod errors;
mod config;


pub use errors::Error;
pub use config::RecoveryMode;

//pub use config::MIN_RECOVERY_READ_BLOCK_SIZE;

mod hosting {
    pub fn add_to_waitlist() {}
}

//mod util;
fn testMod(){
    // let _ = super::util::KIB; 
    // let _ = util::KIB; 
}

pub fn eat_at_restaurant() ->Error {
    hosting::add_to_waitlist();
    
    return Error::InvalidArgument(String::from("error!!!!"));
}