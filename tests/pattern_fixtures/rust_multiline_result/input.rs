fn process(x: i32) -> Result<i32, Error> {
    let y = x + 1;
    let z = y * 2;
    Ok(z)
}

fn quick(a: u8) -> bool {
    a > 0
}
