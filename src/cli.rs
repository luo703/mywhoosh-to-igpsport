use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};
use zeroize::Zeroizing;

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Command {
    /// 执行一轮同步后退出
    Once,
    /// 持续同步，每轮结束后等待 interval 秒（默认）
    Run,
    /// 检查两端登录凭证并读取 FTP，不同步活动
    Check,
    /// 查看本地同步统计和最近 20 条记录，不连接网络
    Status,
    /// 重新登录两端并保存登录凭证，不同步活动
    Login,
}

#[derive(Parser)]
#[command(
    version,
    about = "MyWhoosh → iGPSPORT 中国站自动同步",
    help_template = "{about}\n\n用法：{usage}\n\n{all-args}{after-help}",
    after_help = r#"命令：
  once   同步一轮后退出
  run    持续检查新活动，每轮等待 --interval 秒（默认命令）
  check  检查 MyWhoosh 和 iGPSPORT 登录凭证
  login  重新登录两个平台
  status 查看本地同步状态

同步行为：
  普通运行会记住已处理的 MyWhoosh 活动。没有新增或待处理活动时，
  不会连接 iGPSPORT。首次运行会检查列表中的现有活动。
  iGPSPORT 中已有相同活动时会跳过；新活动才会下载 FIT 并继续同步。
  --resync 强制重新检查 MyWhoosh 活动，并仍检查 iGPSPORT 是否已有重复记录。
  --dry-run 预演同步，不上传文件。

示例：
  mywhoosh-to-igpsport
  mywhoosh-to-igpsport check
  mywhoosh-to-igpsport once --dry-run
  mywhoosh-to-igpsport once --limit 1
  mywhoosh-to-igpsport once --resync --dry-run
  mywhoosh-to-igpsport once --since 2026-10-01
  mywhoosh-to-igpsport run --interval 900
  mywhoosh-to-igpsport status"#
)]
pub struct Args {
    #[arg(
        value_enum,
        default_value = "run",
        value_name = "命令",
        help = "运行方式，详见下方命令说明"
    )]
    pub command: Command,
    #[arg(
        long = "myid",
        value_name = "账号",
        help = "MyWhoosh 账号"
    )]
    pub mywhoosh_username: Option<String>,
    #[arg(
        long = "mypw",
        hide_possible_values = true,
        value_name = "密码",
        help = "MyWhoosh 密码；不会保存在本地"
    )]
    pub mywhoosh_password: Option<String>,
    #[arg(
        long = "igpid",
        value_name = "账号",
        help = "iGPSPORT 中国站账号"
    )]
    pub igpsport_username: Option<String>,
    #[arg(
        long = "igppw",
        hide_possible_values = true,
        value_name = "密码",
        help = "iGPSPORT 密码；不会保存在本地"
    )]
    pub igpsport_password: Option<String>,
    #[arg(
        long,
        value_name = "目录",
        help = "登录凭证和同步数据的保存目录；默认使用系统用户数据目录"
    )]
    pub data_dir: Option<PathBuf>,
    #[arg(
        long,
        value_name = "16位十六进制",
        help = "指定 MyWhoosh 设备 ID；通常无需设置"
    )]
    pub device_id: Option<String>,
    #[arg(
        long,
        default_value = "Asia/Shanghai",
        value_name = "时区",
        help = "运动记录使用的时区，如 Asia/Shanghai"
    )]
    pub timezone: chrono_tz::Tz,
    #[arg(
        long,
        default_value = "室内骑行",
        value_name = "名称",
        help = "上传后显示的活动名称"
    )]
    pub activity_title: String,
    #[arg(long, value_name = "YYYY-MM-DD", help = "只处理此日期及之后的活动")]
    pub since: Option<chrono::NaiveDate>,
    #[arg(long, default_value_t = 900, value_name = "秒", help = "持续运行时两轮检查的间隔，至少 60 秒", value_parser = clap::value_parser!(u64).range(60..))]
    pub interval: u64,
    #[arg(long, default_value_t = 60, value_name = "秒", help = "网络请求超时时间，范围 5–180 秒", value_parser = clap::value_parser!(u64).range(5..=180))]
    pub timeout: u64,
    #[arg(long, default_value_t = 3, value_name = "次数", help = "可重试请求的重试次数，范围 0–5", value_parser = clap::value_parser!(u8).range(0..=5))]
    pub retries: u8,
    #[arg(long, default_value_t = 1000, value_name = "页数", help = "每轮最多读取的活动列表页数，至少 1", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_pages: u32,
    #[arg(long, value_name = "条数", help = "每轮最多处理的活动数，至少 1", value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: Option<u32>,
    #[arg(long, value_name = "活动ID", help = "只处理指定的 MyWhoosh 活动")]
    pub activity_id: Option<String>,
    #[arg(
        long,
        help = "忽略本地活动缓存，重新检查活动；仍检查 iGPSPORT 中是否已有记录"
    )]
    pub resync: bool,
    #[arg(long, help = "预演同步，不上传 FIT 文件")]
    pub dry_run: bool,
}

pub struct Credentials {
    username: Option<String>,
    password: Option<Zeroizing<String>>,
}

impl Credentials {
    pub fn new(username: Option<String>, password: Option<String>) -> Self {
        Self {
            username,
            password: password.map(Zeroizing::new),
        }
    }
    pub fn get(&mut self, service: &str) -> Result<(&str, &str)> {
        if self.username.as_ref().is_none_or(|s| s.trim().is_empty()) {
            if !io::stdin().is_terminal() {
                bail!("{service} 需要账号；请在交互终端启动或传入 CLI 账号密码参数。");
            }
            print!("{service} 账号：");
            io::stdout().flush()?;
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            self.username = Some(line.trim().to_owned());
        }
        if self.password.is_none() {
            if !io::stdin().is_terminal() {
                bail!("{service} 需要密码；请在交互终端启动或传入 CLI 密码参数。");
            }
            self.password = Some(Zeroizing::new(
                rpassword::prompt_password(format!("{service} 密码：")).context("读取密码失败")?,
            ));
        }
        let user = self.username.as_deref().unwrap();
        let pass = self.password.as_ref().unwrap().as_str();
        if user.is_empty() || pass.is_empty() {
            bail!("{service} 账号密码不能为空。");
        }
        Ok((user, pass))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passwords_are_cli_only_and_not_debug_serializable() {
        let mut args = Args::try_parse_from([
            "mywhoosh-to-igpsport",
            "once",
            "--resync",
            "--limit",
            "1",
            "--myid",
            "test@example.test",
            "--mypw",
            "special# $!",
            "--igpid",
            "account",
            "--igppw",
            "other# $",
        ])
        .unwrap();
        assert!(args.resync);
        assert_eq!(args.limit, Some(1));
        let mut credentials =
            Credentials::new(args.mywhoosh_username.take(), args.mywhoosh_password.take());
        assert_eq!(
            credentials.get("test").unwrap(),
            ("test@example.test", "special# $!")
        );
        assert!(args.mywhoosh_password.is_none());
    }
    #[test]
    fn env_file_is_not_a_cli_option() {
        assert!(Args::try_parse_from(["mywhoosh-to-igpsport", "--env", ".env"]).is_err());
    }
}
