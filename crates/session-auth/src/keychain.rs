//! Login Keychain adapter. This process must explicitly disable Keychain interaction.
//! No ACL changes, unlock, private export, or Secure Enclave claim.
use crate::{hex, Identity};
use anyhow::{bail, ensure, Context, Result};
use core_foundation::{
    array::CFArray,
    base::{CFType, CFTypeRef, TCFType},
    boolean::CFBoolean,
    data::CFData,
    dictionary::CFDictionary,
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use p256::ecdsa::Signature;
use security_framework::{
    key::{Algorithm, SecKey},
    os::macos::keychain::SecKeychain,
};
use security_framework_sys::{
    item::*, keychain::SecKeychainSetUserInteractionAllowed, keychain_item::SecItemCopyMatching,
};
extern "C" {
    static kSecAttrApplicationTag: CFStringRef;
    static kSecAttrIsExtractable: CFStringRef;
    static kSecUseAuthenticationUIFail: CFStringRef;
}
fn attr(k: CFStringRef, v: CFType) -> (CFString, CFType) {
    (unsafe { CFString::wrap_under_get_rule(k) }, v)
}
fn string(k: CFStringRef, v: CFStringRef) -> (CFString, CFType) {
    attr(k, unsafe { CFString::wrap_under_get_rule(v) }.into_CFType())
}
pub fn disable_interaction_for_process() -> Result<()> {
    let status = unsafe { SecKeychainSetUserInteractionAllowed(0) };
    ensure!(
        status == 0,
        "cannot disable Keychain interaction ({status})"
    );
    Ok(())
}
fn no_ui() -> Result<()> {
    ensure!(
        !SecKeychain::user_interaction_allowed()?,
        "Keychain interaction must be disabled explicitly by process owner"
    );
    Ok(())
}
fn tag(namespace: &str) -> Result<Vec<u8>> {
    super::identifier(namespace)?;
    Ok(format!("org.veynode.session-auth.v1.{namespace}").into_bytes())
}
pub struct KeychainIdentity {
    key: SecKey,
}
impl KeychainIdentity {
    fn lookup(namespace: &str) -> Result<Option<SecKey>> {
        no_ui()?;
        let tag = tag(namespace)?;
        let pairs = unsafe {
            vec![
                string(kSecClass, kSecClassKey),
                string(kSecAttrKeyClass, kSecAttrKeyClassPrivate),
                string(kSecAttrKeyType, kSecAttrKeyTypeECSECPrimeRandom),
                attr(
                    kSecAttrApplicationTag,
                    CFData::from_buffer(&tag).into_CFType(),
                ),
                attr(kSecReturnRef, CFBoolean::true_value().into_CFType()),
                string(kSecMatchLimit, kSecMatchLimitAll),
                string(kSecUseAuthenticationUI, kSecUseAuthenticationUIFail),
            ]
        };
        let query = CFDictionary::from_CFType_pairs(&pairs);
        let mut reference = std::ptr::null();
        let status = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut reference) };
        if status == -25300 {
            return Ok(None);
        };
        ensure!(status==0,"Keychain lookup failed ({status}); unlock/access approval must be performed locally outside this process");
        ensure!(!reference.is_null(), "Keychain returned empty reference");
        let matches: CFArray<CFTypeRef> =
            unsafe { CFArray::wrap_under_create_rule(reference as _) };
        ensure!(
            matches.len() == 1,
            "ambiguous Keychain identity tag; refusing replacement"
        );
        Ok(Some(unsafe {
            SecKey::wrap_under_get_rule(*matches.get(0).context("missing key")? as _)
        }))
    }
    pub fn open(namespace: &str) -> Result<Self> {
        let key = Self::lookup(namespace)?
            .context("host identity missing; explicit local init required")?;
        let result = Self { key };
        result.validate_private()?;
        Ok(result)
    }
    pub fn create(namespace: &str) -> Result<Self> {
        ensure!(
            Self::lookup(namespace)?.is_none(),
            "identity already exists; refusing replacement"
        );
        let tag = tag(namespace)?;
        let private = unsafe {
            CFDictionary::from_CFType_pairs(&[
                attr(kSecAttrIsPermanent, CFBoolean::true_value().into_CFType()),
                attr(
                    kSecAttrIsExtractable,
                    CFBoolean::false_value().into_CFType(),
                ),
                attr(
                    kSecAttrApplicationTag,
                    CFData::from_buffer(&tag).into_CFType(),
                ),
            ])
        };
        let attributes = unsafe {
            CFDictionary::from_CFType_pairs(&[
                string(kSecAttrKeyType, kSecAttrKeyTypeECSECPrimeRandom),
                attr(kSecAttrKeySizeInBits, CFNumber::from(256).into_CFType()),
                attr(
                    kSecAttrIsExtractable,
                    CFBoolean::false_value().into_CFType(),
                ),
                attr(
                    kSecAttrLabel,
                    CFString::new("Veynode host session identity").into_CFType(),
                ),
                attr(kSecPrivateKeyAttrs, private.into_CFType()),
                string(kSecUseAuthenticationUI, kSecUseAuthenticationUIFail),
            ])
        };
        #[allow(deprecated)]
        let key = SecKey::generate(attributes.to_untyped())
            .map_err(|e| anyhow::anyhow!("Keychain creation failed: {e}"))?;
        let result = Self { key };
        // Apple's file-keychain CDSA path reads extractability only from the top-level
        // dictionary (GetKeyParameters). Keep both levels and verify actual attributes.
        let validation = (|| {
            result.validate_private()?;
            let reopened = Self::lookup(namespace)?.context("generated key not persisted")?;
            ensure!(
                Self { key: reopened }.public_key()? == result.public_key()?,
                "generated identity changed"
            );
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(e) = validation {
            result
                .key
                .delete()
                .context("new invalid key cleanup failed")?;
            bail!("new key rejected and removed: {e}")
        };
        Ok(result)
    }
    pub fn validate_private(&self) -> Result<()> {
        let attributes = self.key.attributes();
        let attribute = unsafe { CFString::wrap_under_get_rule(kSecAttrIsExtractable) };
        let found = attributes
            .find(attribute.as_CFTypeRef())
            .context("Keychain extractability attribute unavailable")?;
        let value = unsafe { CFType::wrap_under_get_rule(*found) };
        ensure!(
            value == CFBoolean::false_value().into_CFType(),
            "Keychain private key is extractable"
        );
        super::identity_id(&self.public_key()?)?;
        Ok(())
    }
    /// Only fixture-created keys expose removal; production identity loss requires explicit external recovery.
    pub fn delete_test_identity(self, namespace: &str) -> Result<()> {
        ensure!(
            namespace.starts_with("test-")
                && Self::open(namespace)?.public_key()? == self.public_key()?,
            "not this test identity"
        );
        no_ui()?;
        self.key.delete()?;
        Ok(())
    }
}
impl Identity for KeychainIdentity {
    fn public_key(&self) -> Result<String> {
        Ok(hex(&self
            .key
            .public_key()
            .context("Keychain public key missing")?
            .external_representation()
            .context("public key export failed")?))
    }
    fn sign(&self, message: &[u8]) -> Result<String> {
        no_ui()?;
        self.validate_private()?;
        let der = self
            .key
            .create_signature(Algorithm::ECDSASignatureMessageX962SHA256, message)
            .map_err(|e| anyhow::anyhow!("Keychain signing failed: {e}"))?;
        Ok(hex(&Signature::from_der(&der)?.to_bytes()))
    }
}
