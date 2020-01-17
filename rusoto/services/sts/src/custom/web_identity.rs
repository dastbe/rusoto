use crate::{
    AssumeRoleWithWebIdentityError, AssumeRoleWithWebIdentityRequest,
    AssumeRoleWithWebIdentityResponse, Sts, StsClient,
};
use futures::{Async, Future, Poll};
use rusoto_core::credential::{
    AwsCredentials, CredentialsError, ProvideAwsCredentials, Secret, Variable,
};
use rusoto_core::request::HttpClient;
use rusoto_core::{Client, Region, RusotoFuture};

const AWS_WEB_IDENTITY_TOKEN_FILE: &str = "AWS_WEB_IDENTITY_TOKEN_FILE";

const AWS_ROLE_ARN: &str = "AWS_ROLE_ARN";

const AWS_ROLE_SESSION_NAME: &str = "AWS_ROLE_SESSION_NAME";

/// WebIdentityProvider using OpenID Connect bearer token to retrieve AWS IAM credentials.
///
/// See https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRoleWithWebIdentity.html for
/// more details.
#[derive(Debug, Clone)]
pub struct WebIdentityProvider {
    /// The OAuth 2.0 access token or OpenID Connect ID token that is provided by the identity provider.
    /// Your application must get this token by authenticating the user who is using your application
    /// with a web identity provider before the application makes an AssumeRoleWithWebIdentity call.
    pub web_identity_token: Variable<Secret, CredentialsError>,
    /// The Amazon Resource Name (ARN) of the role that the caller is assuming.
    pub role_arn: Variable<String, CredentialsError>,
    /// An identifier for the assumed role session. Typically, you pass the name or identifier that is
    /// associated with the user who is using your application. That way, the temporary security credentials
    /// that your application will use are associated with that user. This session name is included as part
    /// of the ARN and assumed role ID in the AssumedRoleUser response element.
    pub role_session_name: Variable<String, CredentialsError>,
}

impl WebIdentityProvider {
    /// Create new WebIdentityProvider by explicitly passing its configuration.
    pub fn new<A, B, C>(web_identity_token: A, role_arn: B, role_session_name: Option<C>) -> Self
    where
        A: Into<Variable<Secret, CredentialsError>>,
        B: Into<Variable<String, CredentialsError>>,
        C: Into<Variable<String, CredentialsError>>,
    {
        Self {
            web_identity_token: web_identity_token.into(),
            role_arn: role_arn.into(),
            role_session_name: role_session_name
                .map(|v| v.into())
                .unwrap_or_else(|| Variable::with_value(Self::create_session_name())),
        }
    }

    /// Creat a WebIdentityProvider from the following environment variables:
    ///
    /// - `AWS_WEB_IDENTITY_TOKEN_FILE` path to the web identity token file.
    /// - `AWS_ROLE_ARN` ARN of the role to assume.
    /// - `AWS_ROLE_SESSION_NAME` (optional) name applied to the assume-role session.
    ///
    /// See https://docs.aws.amazon.com/eks/latest/userguide/iam-roles-for-service-accounts-technical-overview.html
    /// for more information about how IAM Roles for Kubernetes Service Accounts works.
    pub fn from_k8s_env() -> Self {
        Self::_from_k8s_env(
            Variable::from_env_var(AWS_WEB_IDENTITY_TOKEN_FILE),
            Variable::from_env_var(AWS_ROLE_ARN),
            Some(Variable::from_env_var(AWS_ROLE_SESSION_NAME)),
        )
    }

    /// Used by unit testing
    pub(crate) fn _from_k8s_env(
        token_file: Variable<String, CredentialsError>,
        role: Variable<String, CredentialsError>,
        session_name: Option<Variable<String, CredentialsError>>,
    ) -> Self {
        Self::new(
            Variable::dynamic(move || Variable::from_text_file(token_file.resolve()?).resolve()),
            role,
            session_name,
        )
    }

    pub(crate) fn load_token(&self) -> Result<Secret, CredentialsError> {
        self.web_identity_token.resolve()
    }

    fn create_session_name() -> String {
        // TODO can we do better here?
        // - Pod service account, Pod name and Pod namespace
        // - EC2 Instance ID if available
        // - IP address if available
        // - ...
        // Having some information in the session name that identifies the client would enable
        // better correlation analysis in CloudTrail.
        "WebIdentitySession".to_string()
    }
}

impl ProvideAwsCredentials for WebIdentityProvider {
    type Future = WebIdentityProviderFuture;

    fn credentials(&self) -> Self::Future {
        WebIdentityProviderFuture {
            state: WebIdentityProviderFutureState::LoadBearerToken(
                self.load_token(),
                self.role_arn.resolve(),
                self.role_session_name.resolve(),
            ),
        }
    }
}

enum WebIdentityProviderFutureState {
    LoadBearerToken(
        Result<Secret, CredentialsError>,
        Result<String, CredentialsError>,
        Result<String, CredentialsError>,
    ),
    ExchangeToken(RusotoFuture<AssumeRoleWithWebIdentityResponse, AssumeRoleWithWebIdentityError>),
}

/// Provides AWS credentials from environment variables as a Future.
pub struct WebIdentityProviderFuture {
    state: WebIdentityProviderFutureState,
}

impl Future for WebIdentityProviderFuture {
    type Item = AwsCredentials;
    type Error = CredentialsError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use crate::custom::credential::NewAwsCredsForStsCreds;
        use WebIdentityProviderFutureState::*;
        match &mut self.state {
            LoadBearerToken(Err(e), _, _) => Err(e.clone()),
            LoadBearerToken(_, Err(e), _) => Err(e.clone()),
            LoadBearerToken(_, _, Err(e)) => Err(e.clone()),
            LoadBearerToken(Ok(token), Ok(role), Ok(session)) => match HttpClient::new() {
                Err(e) => Err(CredentialsError::new(e.to_string())),
                Ok(c) => {
                    let client = Client::new_not_signing(c);
                    let sts = StsClient::new_with_client(client, Region::default());
                    let mut req = AssumeRoleWithWebIdentityRequest::default();
                    req.role_arn = role.clone();
                    req.web_identity_token = token.as_ref().to_string();
                    req.role_session_name = session.clone();
                    self.state = ExchangeToken(sts.assume_role_with_web_identity(req));
                    self.poll()
                }
            },
            ExchangeToken(ref mut future) => match future.poll() {
                Ok(Async::Ready(r)) => match r.credentials {
                    Some(c) => AwsCredentials::new_for_credentials(c).map(|c| Async::Ready(c)),
                    None => Err(CredentialsError::new(format!(
                        "No credentials found in AssumeRoleWithWebIdentityResponse: {:?}",
                        r
                    ))),
                },
                Ok(Async::NotReady) => Ok(Async::NotReady),
                Err(e) => Err(CredentialsError::new(e.to_string())),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn api_ergonomy() {
        WebIdentityProvider::new(Secret::from("".to_string()), "", Some("".to_string()));
    }

    #[test]
    fn from_k8s_env() -> Result<(), CredentialsError> {
        const TOKEN_VALUE: &str = "secret";
        const ROLE_ARN: &str = "role";
        const SESSION_NAME: &str = "session";
        let mut file = NamedTempFile::new()?;
        // We use writeln to add an extra newline at the end of the token, which should be
        // removed by Variable::from_text_file.
        writeln!(file, "{}", TOKEN_VALUE)?;
        let p = WebIdentityProvider::_from_k8s_env(
            Variable::with_value(file.path().to_string_lossy().to_string()),
            Variable::with_value(ROLE_ARN.to_string()),
            Some(Variable::with_value(SESSION_NAME.to_string())),
        );
        let token = p.load_token()?;
        assert_eq!(token.as_ref(), TOKEN_VALUE);
        Ok(())
    }
}
