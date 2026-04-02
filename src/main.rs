use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut shell = epsh::eval::Shell::new();

    if args.len() > 1 {
        // Process options
        let mut i = 1;
        while i < args.len() {
            if args[i] == "-c" {
                // Execute command string
                if i + 1 >= args.len() {
                    eprintln!("epsh: -c requires an argument");
                    std::process::exit(2);
                }
                let script = &args[i + 1];
                // Remaining args become $0, $1, ...
                if i + 2 < args.len() {
                    shell.set_args(
                        &args[i + 2..]
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<&str>>(),
                    );
                }
                let status = shell.run_script(script);
                std::process::exit(status);
            } else if args[i] == "-s" {
                // Read from stdin
                break;
            } else if args[i].starts_with('-') && args[i].len() > 1 {
                // Shell options like -e, -x, etc.
                for ch in args[i][1..].chars() {
                    match ch {
                        'e' => shell.opts.errexit = true,
                        'u' => shell.opts.nounset = true,
                        'x' => shell.opts.xtrace = true,
                        _ => {
                            eprintln!("epsh: unknown option: -{ch}");
                            std::process::exit(2);
                        }
                    }
                }
                i += 1;
            } else {
                // Script file
                let filename = &args[i];
                shell.set_args(&args[i..].iter().map(|s| s.as_str()).collect::<Vec<&str>>());
                let content = match std::fs::read_to_string(filename) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("epsh: {filename}: {e}");
                        std::process::exit(127);
                    }
                };
                let status = shell.run_script(&content);
                std::process::exit(status);
            }
        }
    }

    // Read from stdin
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("epsh: {e}");
        std::process::exit(1);
    }
    let status = shell.run_script(&input);
    std::process::exit(status);
}
