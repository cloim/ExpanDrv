// WinFsp DLL은 실행 시 설치 경로에서 로드하므로 지연 로드로 링크합니다.
fn main() {
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
}
