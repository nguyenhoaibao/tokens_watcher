use anyhow::{anyhow, Result};
use hex_literal::hex;
use reqwest;
use serde_derive::Deserialize;
use teloxide::{
    adaptors::DefaultParseMode,
    prelude::*,
    types::ParseMode,
    utils::{command::BotCommand, markdown},
};
use tokio::signal;
use tokio::sync::oneshot;
use tokio::time::{interval, Duration};
use web3::{
    contract::{Contract, Options},
    futures::StreamExt,
    types::{FilterBuilder, H160, H256, BlockNumber},
};

use std::error::Error;
use std::sync::Arc;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
struct PancakePriceData {
    price: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OneInchResponse {
    from_token_amount: String,
    to_token_amount: String,
}

#[derive(Debug, Deserialize)]
struct PancakeResponse {
    updated_at: u64,
    data: PancakePriceData,
}

#[derive(Debug)]
enum PriceFeed {
    Pancake,
    OneInch,
}

impl PriceFeed {
    fn api_endpoint(&self) -> &'static str {
        match self {
            Self::Pancake => "https://api.pancakeswap.info/api/v2/tokens/",
            Self::OneInch => "https://api.1inch.io/v4.0/56/quote",
        }
    }

    fn price_endpoint(&self, address: &str) -> String {
        let mut url = reqwest::Url::parse(self.api_endpoint()).unwrap();
        let endpoint: String;
        match self {
            Self::Pancake => endpoint = format!("{}{}", url.as_str(), address),
            Self::OneInch => {
                url.query_pairs_mut()
                    .append_pair("fromTokenAddress", address)
                    .append_pair(
                        "toTokenAddress",
                        "0xe9e7cea3dedca5984780bafc599bd69add087d56", // BUSD
                    )
                    .append_pair("amount", "1000");
                endpoint = url.to_string();
            }
        }
        endpoint
    }

    pub async fn current_price(&self, address: &str) -> Result<f32> {
        let price_endpoint = self.price_endpoint(address);
        match self {
            Self::Pancake => {
                let json: PancakeResponse = reqwest::get(price_endpoint).await?.json().await?;
                let price: f32 = json.data.price.parse()?;
                Ok(price)
            }
            Self::OneInch => {
                let json: OneInchResponse = reqwest::get(price_endpoint).await?.json().await?;
                let from_amount: f32 = json.from_token_amount.parse()?;
                let to_amount: f32 = json.to_token_amount.parse()?;
                Ok(to_amount / from_amount)
            }
        }
    }
}

#[derive(Debug)]
struct Token {
    name: &'static str,
    address: &'static str,
    price_feed: PriceFeed,
    buy_price: f32,
    alert_thresold: f32,
}

impl Token {
    async fn current_price(&self) -> Result<f32> {
        self.price_feed.current_price(self.address).await
    }

    async fn diff_pct(&self) -> Result<(f32, f32, String)> {
        let current_price = self.current_price().await?;
        let mut sign: char = '\0';
        if current_price > self.buy_price {
            sign = '+';
        }
        let mut pct: f32 = 0.0;
        if self.buy_price > 0.0 {
            pct = (current_price - self.buy_price) / self.buy_price * 100.0;
            pct = (pct * 100.0).round() / 100.0;
        }

        return Ok((current_price, pct, format!("{}{}%", sign, pct)));
    }

    fn report_string(&self, current_price: f32, pct_txt: &str) -> String {
        format!(
            "name: {}, buy_price: {}, current_price: {}, diff: {}",
            self.name, self.buy_price, current_price, pct_txt,
        )
    }

    async fn report(&self) -> Result<String> {
        let (current_price, _, pct_txt) = self.diff_pct().await?;
        Ok(self.report_string(current_price, &pct_txt))
    }

    async fn check(&self) -> Result<String> {
        let (current_price, pct, pct_txt) = self.diff_pct().await?;
        if pct > 0.0 && pct < self.alert_thresold {
            return Ok("".to_owned());
        } else if pct <= 0.0 && pct > -self.alert_thresold {
            return Ok("".to_owned());
        }
        Ok(self.report_string(current_price, &pct_txt))
    }
}

struct Reporter {
    tokens: Vec<Token>,
}

impl Reporter {
    async fn report(&self) -> Result<String> {
        let mut ret = String::new();
        for token in &self.tokens {
            let txt = token.report().await;
            match txt {
                Ok(txt) => ret.push_str(&txt),
                Err(err) => ret.push_str(
                    format!(
                        "fail to diff pct for token: {}, got error: {}",
                        token.name, err
                    )
                    .as_str(),
                ),
            };
            ret.push_str("\n");
        }
        return Ok(ret);
    }

    async fn check(&self) -> Result<String> {
        let mut md = String::new();
        for token in &self.tokens {
            let txt = token.check().await;
            match txt {
                Ok(txt) => {
                    if txt != "" {
                        md.push_str(&txt);
                        md.push_str("\n");
                    }
                }
                Err(err) => md.push_str(
                    format!(
                        "fail to diff pct for token: {}, got error: {}\n",
                        token.name, err
                    )
                    .as_str(),
                ),
            }
        }
        if md == "" {
            return Ok("".to_owned());
        }

        let mut ret = String::from("**ALERT**");
        ret.push_str("\n");
        ret.push_str(&markdown::code_block(&md));
        Ok(ret)
    }

    async fn cmd(
        &self,
        cx: UpdateWithCx<Arc<AutoSend<DefaultParseMode<Bot>>>, Message>,
        command: Command,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        match command {
            Command::P => {
                let ret = self.report().await;
                let txt;
                match ret {
                    Ok(data) => txt = data,
                    Err(err) => txt = format!("fail to report: {}", err),
                };
                cx.answer(markdown::code_block(&txt))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
        };

        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }

    async fn watch(
        &self,
        bot: Arc<AutoSend<DefaultParseMode<Bot>>>,
        chat_id: i64,
        mut recv: oneshot::Receiver<String>,
    ) {
        let mut interval = interval(Duration::from_secs(60 * 15));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    println!("checking price...");
                    let txt = self.check().await.unwrap();
                    if txt == "" {
                        continue;
                    }
                    match bot.send_message(chat_id, &txt).parse_mode(ParseMode::MarkdownV2).await {
                        Ok(_) => {},
                        Err(err) => println!("got error when sending message: {}", err)
                    }
                },
                msg = &mut recv => {
                    println!("got message: {}", msg.unwrap());
                    break;
                }
            }
        }
    }
}

#[derive(BotCommand, Debug)]
#[command(rename = "lowercase", prefix = "P")]
enum Command {
    P,
}

async fn xx() -> Result<()> {
    let web3 = web3::Web3::new(web3::transports::Http::new("https://bsc-dataseed2.defibit.io/")?);
    let addr = H160::from_str("0xF339E8c294046E6E7ef6AD4F6fa9E202B59b556B").unwrap();
    println!("addr {}", addr);

    let w = H160::from_str("0x589b483486c4320C66Cc0ff9FE3A74d31cb9FC37").unwrap();

    let t0 = H256::from_str("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef").unwrap();
    println!("t0 {}", t0);
    // let t1 = H256::from_str("0x589b483486c4320c66cc0ff9fe3a74d31cb9fc37").unwrap();
    // println!("t1 {}", t1);

    // Filter for Hello event in our contract
    let filter = FilterBuilder::default()
        .address(vec![addr])
        .topics(
            Some(vec![
                t0
            .into()]),
            None,
            None,
            None,
        )
        .from_block(BlockNumber::Number(web3::types::U64([14360291])))
        .build();

    let filter = web3.eth_filter().create_logs_filter(filter).await?;

    let logs_stream = filter.stream(Duration::from_secs(1));
    web3::futures::pin_mut!(logs_stream);

    let log = logs_stream.next().await.unwrap().unwrap();
    println!("got log: {:?}", log);

    let t1 = log.topics[1].to_string();
    println!("t1 ne {}", t1);

    Ok(())
}

#[tokio::main]
async fn main() {
    run().await;
}

async fn run() {
    teloxide::enable_logging!();
    log::info!("Starting bot...");

    let chat_id = std::env::var("CHAT_ID")
        .expect("cannot get CHAT_ID env")
        .parse::<i64>()
        .unwrap();

    // let (send, mut recv) = oneshot::channel();

    let bot = Arc::new(
        Bot::from_env()
            .parse_mode(ParseMode::MarkdownV2)
            .auto_send(),
    );

    let reporter = Reporter {
        tokens: vec![
            Token {
                name: "BGS",
                address: "0xf339e8c294046e6e7ef6ad4f6fa9e202b59b556b",
                price_feed: PriceFeed::Pancake,
                buy_price: 0.03,
                alert_thresold: 30.0,
            },
            Token {
                name: "ILA",
                address: "0x4fBEdC7b946e489208DED562e8E5f2bc83B7de42",
                price_feed: PriceFeed::Pancake,
                buy_price: 0.01,
                alert_thresold: 1200.0,
            },
            Token {
                name: "WOO",
                address: "0x4691937a7508860f876c9c0a2a617e7d9e945d4b",
                price_feed: PriceFeed::Pancake,
                buy_price: 0.75,
                alert_thresold: 100.0,
            },
            Token {
                name: "SPARTA",
                address: "0x3910db0600ea925f63c36ddb1351ab6e2c6eb102",
                price_feed: PriceFeed::OneInch,
                buy_price: 0.0,
                alert_thresold: 30.0,
            },
        ],
    };

    let reporter = Arc::new(reporter);
    xx().await;

    // {
    //     let bot = Arc::clone(&bot);
    //     let reporter = Arc::clone(&reporter);
    //     tokio::spawn(async move {
    //         reporter.watch(bot, chat_id, recv).await;
    //     });
    // }
    // teloxide::commands_repl(Arc::clone(&bot), "fw", move |cx, command| {
    //     let reporter = Arc::clone(&reporter);
    //     async move { reporter.cmd(cx, command).await }
    // })
    // .await;
    //
    // send.send("shutdown".to_owned()).unwrap();
}
