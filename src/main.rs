mod cli;
mod db;
mod fit;
mod net;
mod sign;
mod source;
mod storage;
mod sync;
mod target;

use anyhow::{ensure, Context, Result};
use clap::Parser;
use cli::{Args, Command, Credentials};

fn run() -> Result<bool> {
    let mut args = Args::parse();
    ensure!(
        !args.resync || matches!(args.command, Command::Once | Command::Run),
        "--resync 仅用于 once/run"
    );
    let directory = args.data_dir.clone().unwrap_or(
        directories::ProjectDirs::from("", "", "mywhoosh-sync")
            .context("无法确定数据目录，请指定 --data-dir")?
            .data_local_dir()
            .to_owned(),
    );
    let _lock = storage::lock(&directory)?;
    let db = db::Db::open(&directory.join("sync.sqlite3"))?;
    if args.command == Command::Status {
        db.status()?;
        return Ok(false);
    }
    let mw = Credentials::new(args.mywhoosh_username.take(), args.mywhoosh_password.take());
    let igp = Credentials::new(args.igpsport_username.take(), args.igpsport_password.take());
    let http = net::Http::new(args.timeout, args.retries)?;
    let mut source = source::Source::new(
        http.clone(),
        directory.join("mywhoosh-session.json"),
        mw,
        args.device_id.clone(),
    )?;
    let mut target = target::Target::new(
        http,
        directory.join("igpsport-session.json"),
        igp,
        args.timezone.name().to_owned(),
    )?;
    println!("数据目录：{}", directory.display());
    println!("[连接 1/2] 正在验证 MyWhoosh 登录凭证并读取个人资料……");
    source.connect(args.command == Command::Login)?;
    if matches!(args.command, Command::Check | Command::Login) {
        println!("[连接 2/2] 正在验证 iGPSPORT 中国站登录凭证……");
        target.connect(args.command == Command::Login)?;
        println!("iGPSPORT 登录凭证有效。");
        println!("完成：两个平台均已连接，登录凭证已保存。");
        return Ok(false);
    }
    let mut target_connected = false;
    loop {
        let result = (|| -> Result<sync::Counts> {
            if args.command == Command::Run {
                // Refresh FTP from the current profile for every polling cycle.
                source.connect(false)?;
                println!("正在刷新 MyWhoosh 个人资料及 FTP（{} W）……", source.ftp);
            }
            let rows = sync::activities_for_cycle(&args, &mut source, &db)?;
            if rows.is_empty() && !args.resync {
                println!("没有新增或待处理的 MyWhoosh 活动。");
                return Ok(sync::Counts::default());
            }
            if !target_connected {
                println!("[连接 2/2] 正在验证 iGPSPORT 中国站登录凭证……");
                target.connect(false)?;
                println!("iGPSPORT 登录凭证有效。");
                target_connected = true;
            }
            sync::once(&args, &directory, &mut source, &mut target, &db, rows)
        })();
        if args.command == Command::Once {
            return Ok(result?.failed());
        }
        if let Err(error) = result {
            eprintln!("本轮同步失败：{error}");
        }
        println!("{} 秒后继续；Ctrl+C 停止。", args.interval);
        std::thread::sleep(std::time::Duration::from_secs(args.interval));
    }
}

fn main() {
    match run() {
        Ok(false) => {}
        Ok(true) => std::process::exit(1),
        Err(error) => {
            eprintln!("错误：{error}");
            std::process::exit(1);
        }
    }
}
