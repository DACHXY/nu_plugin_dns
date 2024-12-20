use std log

def main [package_file: path] {
    
    let repo_root = $package_file | path dirname
    let install_root = $env.NUPM_HOME | path join "plugins"

    let name = open ($repo_root | path join "Cargo.toml") | get package.name
    let cmd = $"cargo install --path ($repo_root) --root ($install_root)"
    log info $"building plugin using: (ansi blue)($cmd)(ansi reset)"
    nu -c $cmd
    let ext: string = if ($nu.os-info.name == 'windows') { '.exe' } else { '' }
    let bin_path = $"($install_root | path join "bin" $name)($ext)"
    plugin add $bin_path

    log info "do not forget to restart Nushell for the plugin to be fully available!"
}
