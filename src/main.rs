use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;

use clap::Parser;

mod chunker;
mod encoding;

use chunker::Chunker;
use encoding::Transcoder;

fn main() {
    let cli = Cli::parse();
    let input: Box<dyn BufRead> = match cli.inputfile {
        None => Box::new(io::stdin().lock()),
        Some(filename) => Box::new(BufReader::new(File::open(filename).unwrap())),
    };
    for chunk in Chunker::new(Transcoder::new(input)) {
        match chunk {
            Err(err) => panic!("chunker error: {}", err),
            Ok(chunk) => println!(
                ">>> START CHUNK ({} bytes) >>>|{}|<<< END CHUNK <<<",
                chunk.len(),
                chunk,
            ),
        }
    }
}

#[derive(Parser)]
struct Cli {
    #[clap(
        name = "inputfile",
        help = "A file to read from instead of standard input"
    )]
    inputfile: Option<PathBuf>,
}
