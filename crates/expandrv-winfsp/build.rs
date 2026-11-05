// 테스트 바이너리도 winfsp DLL을 지연 로드해야 실행 시 PATH에 DLL이 없어도 시작됩니다.
fn main() {
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
}
