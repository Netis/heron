use clap::Parser;
use duckdb::Connection;

mod calls;
mod metrics;
mod turns;
mod ui;

use ui::{clear_screen, fmt_tokens, read_line, AltScreenGuard};

#[derive(Parser)]
#[command(
    name = "ts-dbview",
    about = "Browse TokenScope DuckDB tables (metrics / calls / turns)"
)]
struct Args {
    /// Path to the DuckDB database file
    #[arg(short, long, default_value = "data/tokenscope.duckdb")]
    db: String,

    /// Maximum number of records to list per table
    #[arg(short, long, default_value_t = 50)]
    limit: usize,
}

fn print_summary(conn: &Connection) {
    let sql = "SELECT COALESCE(SUM(request_count), 0), \
               COALESCE(SUM(total_input_tokens), 0), \
               COALESCE(SUM(total_output_tokens), 0), \
               COALESCE(SUM(error_count), 0) \
               FROM llm_metrics WHERE provider = '*' AND model = '*' AND server_ip = '*'";
    let (in_tokens, out_tokens, err_count) = conn
        .prepare(sql)
        .and_then(|mut s| {
            s.query_row([], |row| {
                Ok((
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            })
        })
        .unwrap_or((0, 0, 0));

    let call_count: u64 = conn
        .prepare("SELECT COUNT(*) FROM llm_calls")
        .and_then(|mut s| s.query_row([], |row| row.get(0)))
        .unwrap_or(0);
    let turn_count: u64 = conn
        .prepare("SELECT COUNT(*) FROM llm_turns")
        .and_then(|mut s| s.query_row([], |row| row.get(0)))
        .unwrap_or(0);

    println!(
        "Turns: {} | Calls: {} | Tokens: {} in / {} out | Errors: {}",
        turn_count,
        call_count,
        fmt_tokens(in_tokens),
        fmt_tokens(out_tokens),
        err_count,
    );
}

fn print_menu(db_path: &str, conn: &Connection) {
    println!("TokenScope DB Viewer — {db_path}");
    print_summary(conn);
    println!();
    println!("  1) llm_metrics   — aggregated time-series");
    println!("  2) llm_calls     — per-request detail");
    println!("  3) llm_turns     — agent interaction turns");
    println!("  q) quit");
    println!();
}

fn main() {
    let args = Args::parse();
    let conn = Connection::open(&args.db).unwrap_or_else(|e| {
        eprintln!("Failed to open database '{}': {e}", args.db);
        std::process::exit(1);
    });

    let _guard = AltScreenGuard::new();

    loop {
        clear_screen();
        print_menu(&args.db, &conn);
        let input = read_line("Choose a table: ");
        match input.as_str() {
            "1" => metrics::run(&conn, args.limit),
            "2" => calls::run(&conn, args.limit),
            "3" => turns::run(&conn, args.limit),
            "q" | "Q" | "" => break,
            _ => {
                println!("Unknown choice: {input}");
                read_line("Press Enter to continue...");
            }
        }
    }
}
