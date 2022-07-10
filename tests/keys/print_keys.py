import os.path
import sys
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey
from cryptography.hazmat.primitives.asymmetric.rsa import RSAPrivateKey, RSAPublicKey
from cryptography.hazmat.primitives.asymmetric.ec import EllipticCurvePrivateKey

def print_keypair(private_key, public_key):
    if isinstance(private_key, Ed25519PrivateKey):
        raw_private = private_key.private_bytes(
            serialization.Encoding.Raw,
            serialization.PrivateFormat.Raw,
            serialization.NoEncryption(),
        )
        raw_public = public_key.public_bytes(
            serialization.Encoding.Raw,
            serialization.PublicFormat.Raw,
        )

        print(f"    let private_bytes = hex!(\"{raw_private.hex()}\");")
        print(f"    let public_bytes = hex!(\"{raw_public.hex()}\");")
        print( "    makiko::Privkey::Ed25519(ed25519_dalek::Keypair {")
        print( "        secret: ed25519_dalek::SecretKey::from_bytes(&private_bytes).unwrap(),")
        print( "        public: ed25519_dalek::PublicKey::from_bytes(&public_bytes).unwrap(),")
        print( "    }.into())")
    elif isinstance(private_key, RSAPrivateKey):
        def print_num(name, x):
            x = to_be_bytes(x)
            if len(x) < 40:
                print(f"    let {name} = BigUint::from_bytes_be(&hex!(\"{x.hex()}\"));")
            else:
                print(f"    let {name} = BigUint::from_bytes_be(&hex!(")
                while x:
                    chunk, x = x[:32], x[32:]
                    print(f"        \"{chunk.hex()}\"")
                print("    ));")

        numbers = private_key.private_numbers()
        print_num("n", numbers.public_numbers.n)
        print_num("e", numbers.public_numbers.e)
        print_num("d", numbers.d)
        print_num("p", numbers.p)
        print_num("q", numbers.q)

        print("    let privkey = rsa::RsaPrivateKey::from_components(n, e, d, vec![p, q]);")
        print("    makiko::Privkey::Rsa(privkey.into())")
    elif isinstance(private_key, EllipticCurvePrivateKey):
        if private_key.curve.name == "secp256r1":
            curve = "p256::NistP256"
            variant = "EcdsaP256"
        elif private_key.curve.name == "secp384r1":
            curve = "p384::NistP384"
            variant = "EcdsaP384"
        else:
            raise NotImplementedError()

        private_numbers = private_key.private_numbers()
        private_bytes = to_be_bytes(private_numbers.private_value)

        print(f"    let private_key = elliptic_curve::SecretKey::<{curve}>::from_be_bytes(&hex!(")
        print(f"        \"{private_bytes.hex()}\"")
        print( "    )).unwrap();")
        print(f"    let privkey = makiko::pubkey::EcdsaPrivkey::<{curve}>::from(private_key);")
        print(f"    makiko::Privkey::{variant}(privkey)")
    else:
        raise NotImplementedError()

def to_be_bytes(x):
    return x.to_bytes((x.bit_length() + 7) // 8, "big")

base_dir = os.path.dirname(__file__)

print(f"// auto generated by {os.path.basename(__file__)}")
print("use num_bigint_dig::BigUint;")
print("use hex_literal::hex;")
print("use makiko::elliptic_curve;")
print()

for name in [
        "alice_ed25519", "edward_ed25519",
        "ruth_rsa_1024", "ruth_rsa_2048", "ruth_rsa_4096",
        "eda_ecdsa_p256", "eda_ecdsa_p384",
        "encrypted_rsa", "encrypted_ed25519",
        "encrypted_ecdsa_p256", "encrypted_ecdsa_p384",
        #"encrypted_rsa_aes128-gcm",
]:
    private_file = os.path.join(base_dir, name)
    public_file = os.path.join(base_dir, f"{name}.pub")
    private_bytes = open(private_file, "rb").read()
    public_bytes = open(public_file, "rb").read()
    private_key = serialization.load_ssh_private_key(private_bytes, b"password")
    public_key = serialization.load_ssh_public_key(public_bytes)

    print(f"pub fn {name}() -> makiko::Privkey {{")
    print_keypair(private_key, public_key)
    print(f"}}")

    private_str = private_bytes.decode("utf-8")
    print(f"pub static {name.upper()}_KEYPAIR_PEM: &'static str = concat!(")
    for line in private_str.splitlines(keepends=True):
        escaped_line = line.translate({
            ord("\n"): "\\n",
            ord("\\"): "\\\\",
            ord("\""): "\"",
        })
        print(f"    \"{escaped_line}\",")
    print(");")

    print()
