use heddle::softfp::*;
fn main(){
  let mut e = Env::enter(0xc00000);
  let r = ffma(&mut e, Ft::H, 0xfac6, 1, 1);
  let f = e.leave();
  println!("{:#x} flags={:#x}", r, f);
  let mut e = Env::enter(0xc00000);
  let r = convert(&mut e, Ft::D, Ft::H, (-55499.99999999999f64).to_bits());
  println!("conv {:#x} flags={:#x}", r, e.leave());
}
