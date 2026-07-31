use url::Url;

const APOLLO_SCHEME: &str = "art";
const APOLLO_LAUNCH_ACTION: &str = "launch";
const MAX_URI_LENGTH: usize = 8 * 1024;
const MAX_FIELD_LENGTH: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApolloLaunchRequest {
    pub host_uuid: String,
    pub host_name: Option<String>,
    pub app_uuid: Option<String>,
    pub app_name: Option<String>,
    pub app_id: Option<i32>,
}

impl ApolloLaunchRequest {
    #[must_use]
    pub fn application_label(&self) -> &str {
        self.app_name
            .as_deref()
            .or(self.app_uuid.as_deref())
            .unwrap_or("requested application")
    }
}

pub fn apollo_launch_from_arguments(
    arguments: &[String],
) -> Result<Option<ApolloLaunchRequest>, String> {
    let mut links = arguments
        .iter()
        .filter(|argument| argument.starts_with("art:"));
    let Some(link) = links.next() else {
        return Ok(None);
    };
    if links.next().is_some() {
        return Err("Only one Apollo launch link may be handled at a time.".to_owned());
    }
    parse_apollo_launch_uri(link).map(Some)
}

fn parse_apollo_launch_uri(value: &str) -> Result<ApolloLaunchRequest, String> {
    if value.len() > MAX_URI_LENGTH {
        return Err("The Apollo launch link is too long.".to_owned());
    }
    let url = Url::parse(value).map_err(|error| format!("Invalid Apollo launch link: {error}"))?;
    if url.scheme() != APOLLO_SCHEME
        || url.host_str() != Some(APOLLO_LAUNCH_ACTION)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !matches!(url.path(), "" | "/")
        || url.fragment().is_some()
    {
        return Err("Expected an art://launch Apollo link.".to_owned());
    }

    let mut host_uuid = None;
    let mut host_name = None;
    let mut app_uuid = None;
    let mut app_name = None;
    let mut app_id = None;
    for (key, value) in url.query_pairs() {
        let value = bounded_field(&key, value.as_ref())?;
        match key.as_ref() {
            "host_uuid" => set_once(&mut host_uuid, value, "host_uuid")?,
            "host_name" => set_once(&mut host_name, value, "host_name")?,
            "app_uuid" => set_once(&mut app_uuid, value, "app_uuid")?,
            "app_name" => set_once(&mut app_name, value, "app_name")?,
            "app_id" => {
                if app_id.is_some() {
                    return Err("Apollo launch link contains duplicate app_id values.".to_owned());
                }
                app_id =
                    Some(value.parse::<i32>().map_err(|_| {
                        "Apollo launch link contains an invalid app_id.".to_owned()
                    })?);
            }
            _ => {}
        }
    }

    let host_uuid = required_field(host_uuid, "host_uuid")?;
    if app_uuid.as_deref().is_none_or(str::is_empty)
        && app_name.as_deref().is_none_or(str::is_empty)
        && app_id.is_none()
    {
        return Err(
            "Apollo launch link must identify an application by UUID, name, or ID.".to_owned(),
        );
    }
    Ok(ApolloLaunchRequest {
        host_uuid,
        host_name: non_empty(host_name),
        app_uuid: non_empty(app_uuid),
        app_name: non_empty(app_name),
        app_id,
    })
}

fn bounded_field(name: &str, value: &str) -> Result<String, String> {
    if value.len() > MAX_FIELD_LENGTH {
        return Err(format!(
            "Apollo launch link field {name} exceeds {MAX_FIELD_LENGTH} bytes."
        ));
    }
    Ok(value.to_owned())
}

fn set_once(target: &mut Option<String>, value: String, name: &str) -> Result<(), String> {
    if target.replace(value).is_some() {
        return Err(format!(
            "Apollo launch link contains duplicate {name} values."
        ));
    }
    Ok(())
}

fn required_field(value: Option<String>, name: &str) -> Result<String, String> {
    non_empty(value).ok_or_else(|| format!("Apollo launch link is missing {name}."))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::{apollo_launch_from_arguments, parse_apollo_launch_uri};

    #[test]
    fn parses_current_apollo_webui_launch_contract() {
        let request = parse_apollo_launch_uri(
            "art://launch?host_uuid=host-123&host_name=Gaming%20PC\
             &app_uuid=app-456&app_name=Steam%20Big%20Picture",
        )
        .expect("valid Apollo link");

        assert_eq!(request.host_uuid, "host-123");
        assert_eq!(request.host_name.as_deref(), Some("Gaming PC"));
        assert_eq!(request.app_uuid.as_deref(), Some("app-456"));
        assert_eq!(request.app_name.as_deref(), Some("Steam Big Picture"));
        assert_eq!(request.app_id, None);
    }

    #[test]
    fn accepts_legacy_numeric_application_id() {
        let request =
            parse_apollo_launch_uri("art://launch?host_uuid=host-123&app_id=42&app_name=Desktop")
                .expect("valid legacy Apollo link");

        assert_eq!(request.app_id, Some(42));
        assert_eq!(request.app_name.as_deref(), Some("Desktop"));
    }

    #[test]
    fn extracts_one_link_from_desktop_arguments() {
        let request = apollo_launch_from_arguments(&[
            "artemis-linux".to_owned(),
            "--ignored".to_owned(),
            "art://launch?host_uuid=host&app_uuid=app".to_owned(),
        ])
        .expect("valid arguments")
        .expect("launch request");

        assert_eq!(request.host_uuid, "host");
        assert_eq!(request.app_uuid.as_deref(), Some("app"));
    }

    #[test]
    fn rejects_untrusted_or_incomplete_links() {
        assert!(parse_apollo_launch_uri("https://launch?host_uuid=host&app_uuid=app").is_err());
        assert!(parse_apollo_launch_uri("art://pair?host_uuid=host&app_uuid=app").is_err());
        assert!(parse_apollo_launch_uri("art://launch?app_uuid=app").is_err());
        assert!(parse_apollo_launch_uri("art://launch?host_uuid=host").is_err());
    }

    #[test]
    fn rejects_duplicate_security_identifiers() {
        let error =
            parse_apollo_launch_uri("art://launch?host_uuid=one&host_uuid=two&app_uuid=app")
                .expect_err("duplicate host");

        assert!(error.contains("duplicate host_uuid"));
    }
}
