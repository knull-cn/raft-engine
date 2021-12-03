
use step1::Error;

fn main(){
    let err = step1::eat_at_restaurant();
    print!("eat_at_restaurant err:{}.",err.to_string());
}