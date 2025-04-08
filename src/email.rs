use aws_config::BehaviorVersion;
use aws_sdk_sesv2::{
    config::ProvideCredentials,
    types::{Body, Content, Destination, EmailContent, Message},
    Client,
};
use lightning_invoice::RouteHint;
#[cfg(test)]
use mockall::automock;
use secp256k1::{hashes::sha256, PublicKey};
use tracing::{debug, error, warn};

pub struct NotifyPaymentFailedRequest {
    pub destination: PublicKey,
    pub error: anyhow::Error,
    pub hints: Vec<RouteHint>,
    pub payment_hash: sha256::Hash,
    pub invoice: String,
}

impl NotifyPaymentFailedRequest {
    pub fn to_html(&self) -> impl Into<String> {
        let hints: Vec<_> = self
            .hints
            .iter()
            .filter_map(|r| r.0.last())
            .enumerate()
            .map(|(i, hint)| {
                format!(
                    "<tr><td>Route hint {}: {}, {}</td></tr>",
                    i,
                    hint.src_node_id,
                    scid_to_string(hint.short_channel_id),
                )
            })
            .collect();
        format!(
            "
            <table>
                <tr><td>Destination: {}</td></tr>{}
                <tr><td>Error: {}</td></tr>
                <tr><td>Payment hash: {}</td></tr>
                <tr><td>Invoice: {}</td></tr>
            </table>",
            self.destination,
            hints.join(""),
            self.error
                .to_string()
                .replace("&", "&amp;")
                .replace("<", "&lt;")
                .replace(">", "&gt;")
                .replace(r#"""#, "&quot;")
                .replace("'", "&#x27;")
                .replace("/", "&#x2F;"),
            self.payment_hash,
            &self.invoice
        )
    }
}

fn scid_to_string(scid: u64) -> String {
    format!(
        "{}x{}x{}",
        scid >> 40,
        (scid >> 16) & 0xFFFFFF,
        scid & 0xFFFF
    )
}

#[cfg_attr(test, automock)]
#[async_trait::async_trait]
pub trait NotificationService {
    async fn notify_payment_failed(&self, req: NotifyPaymentFailedRequest);
}

#[derive(Debug, Clone)]
pub struct EmailNotificationService {
    config: Option<Config>,
}

#[derive(Debug, Clone)]
struct Config {
    client: Client,
    params: EmailParams,
}

#[derive(Debug, Clone)]
pub struct EmailParams {
    pub from: String,
    pub destination: Destination,
    pub subject: String,
}

impl EmailNotificationService {
    pub async fn new(params: Option<EmailParams>) -> Self {
        let params = match params {
            Some(params) => params,
            None => {
                debug!("did not find email options. Disabling email notifications.");
                return Self { config: None };
            }
        };

        let aws_config = aws_config::load_defaults(BehaviorVersion::v2024_03_28()).await;

        let cp = match aws_config.credentials_provider() {
            Some(cp) => cp,
            None => {
                warn!("did not locate AWS credentials. Disabling email notifications.");
                return Self { config: None };
            }
        };

        if let Err(e) = cp.provide_credentials().await {
            warn!(
                "did not locate AWS credentials. Disabling email notifications: {:?}",
                e
            );
            return Self { config: None };
        };

        debug!("enabling email notifications.");
        let client = Client::new(&aws_config);

        Self {
            config: Some(Config { client, params }),
        }
    }
}

#[async_trait::async_trait]
impl NotificationService for EmailNotificationService {
    async fn notify_payment_failed(&self, req: NotifyPaymentFailedRequest) {
        let config = match &self.config {
            Some(config) => config,
            None => return,
        };
        match notify(config, req).await {
            Ok(_) => debug!("sent payment failure notification email"),
            Err(e) => error!("failed to send payment failure notification email: {:?}", e),
        };
    }
}

async fn notify(
    config: &Config,
    req: NotifyPaymentFailedRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let html_content = Content::builder()
        .charset("UTF-8")
        .data(req.to_html())
        .build()?;
    let subject = Content::builder()
        .charset("UTF-8")
        .data(config.params.subject.clone())
        .build()?;
    let body = Body::builder().html(html_content).build();
    let message = Message::builder().body(body).subject(subject).build();
    let email_content = EmailContent::builder().simple(message).build();

    config
        .client
        .send_email()
        .destination(config.params.destination.clone())
        .from_email_address(config.params.from.clone())
        .content(email_content)
        .send()
        .await?;
    Ok(())
}
