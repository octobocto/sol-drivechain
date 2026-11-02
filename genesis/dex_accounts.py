"""The genesis accounts of the DEX, built from constants, never copied.

  python3 -I genesis/dex_accounts.py no-key
  python3 -I genesis/dex_accounts.py accounts <lamports per byte year> <out>
  python3 -I genesis/dex_accounts.py self-test
"""

import base64
import hashlib
import struct
import sys

ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
P = 2**255 - 19
D = (-121665 * pow(121666, P - 2, P)) % P

NO_KEY_SEED = b"sol-drivechain: nobody holds this key"
TOKEN_ID = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
CPMM_ID = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"
CPMM_CONFIG_ID = "D4FPEruKEHrG5TenZ2mpDGEfu1iUvTiqBxvpU8HLBvC2"
CPMM_FEE_ID = "DNXgeM9EiiaAbaWvwjHj9fQQLAX5ZsfHyvmYUNRAdNC8"
WSOL_ID = "So11111111111111111111111111111111111111112"

# 25 basis points of each trade, all to the liquidity providers.
TRADE_FEE_RATE = 2500
AMM_CONFIG_LEN = 236
MINT_LEN = 82
TOKEN_ACCOUNT_LEN = 165


def b58decode(text):
    if not 32 <= len(text) <= 44 or any(c not in ALPHABET for c in text):
        raise ValueError(f"{text!r} is not a base58 pubkey")
    number = 0
    for character in text:
        number = number * 58 + ALPHABET.index(character)
    body = number.to_bytes(32, "big")
    if b58encode(body) != text:
        raise ValueError(f"{text!r} is not a canonical 32 byte pubkey")
    return body


def b58encode(raw):
    number = int.from_bytes(raw, "big")
    text = ""
    while number:
        number, rest = divmod(number, 58)
        text = ALPHABET[rest] + text
    return "1" * (len(raw) - len(raw.lstrip(b"\0"))) + text


def on_curve(raw):
    y = int.from_bytes(raw, "little") & ((1 << 255) - 1)
    u = (y * y - 1) % P
    v = (D * y * y + 1) % P
    x2 = u * pow(v, P - 2, P) % P
    return x2 == 0 or pow(x2, (P - 1) // 2, P) == 1


def no_key():
    raw = hashlib.sha256(NO_KEY_SEED + bytes([0])).digest()
    if on_curve(raw):
        raise AssertionError("the no-key hash is on the curve")
    return b58encode(raw)


def program_address(seeds, program_id):
    program = b58decode(program_id)
    for bump in range(255, -1, -1):
        raw = hashlib.sha256(
            b"".join(seeds) + bytes([bump]) + program + b"ProgramDerivedAddress"
        ).digest()
        if not on_curve(raw):
            return b58encode(raw), bump
    raise AssertionError("no program address")


def rent(data_len, lamports_per_byte_year):
    return (128 + data_len) * lamports_per_byte_year * 2


def amm_config(bump, owner):
    discriminator = hashlib.sha256(b"account:AmmConfig").digest()[:8]
    body = discriminator + struct.pack(
        "<B?HQQQQ32s32sQQ112s",
        bump,
        False,
        0,
        TRADE_FEE_RATE,
        0,
        0,
        0,
        b58decode(owner),
        b58decode(owner),
        0,
        0,
        bytes(112),
    )
    assert len(body) == AMM_CONFIG_LEN, len(body)
    return body


def native_mint():
    body = struct.pack("<I32sQB?I32s", 0, bytes(32), 0, 9, True, 0, bytes(32))
    assert len(body) == MINT_LEN, len(body)
    return body


def native_token_account(mint, owner, reserve):
    body = struct.pack(
        "<32s32sQI32sBIQQI32s",
        b58decode(mint),
        b58decode(owner),
        0,
        0,
        bytes(32),
        1,
        1,
        reserve,
        0,
        0,
        bytes(32),
    )
    assert len(body) == TOKEN_ACCOUNT_LEN, len(body)
    return body


def entry(key, balance, owner, data):
    b58decode(key)
    b58decode(owner)
    if not isinstance(balance, int) or balance <= 0:
        raise ValueError(f"bad balance {balance!r}")
    encoded = base64.b64encode(data).decode("ascii")
    return (
        f"{key}:\n"
        f"  balance: {balance}\n"
        f"  owner: {owner}\n"
        f'  data: "{encoded}"\n'
        f"  executable: false\n"
    )


def accounts(lamports_per_byte_year):
    owner = no_key()
    config_key, bump = program_address(
        [b"amm_config", struct.pack(">H", 0)], CPMM_ID
    )
    if config_key != CPMM_CONFIG_ID:
        raise AssertionError(f"the config PDA is {config_key}")
    fee_reserve = rent(TOKEN_ACCOUNT_LEN, lamports_per_byte_year)
    return (
        entry(
            CPMM_CONFIG_ID,
            rent(AMM_CONFIG_LEN, lamports_per_byte_year),
            CPMM_ID,
            amm_config(bump, owner),
        )
        + entry(
            WSOL_ID, rent(MINT_LEN, lamports_per_byte_year), TOKEN_ID, native_mint()
        )
        + entry(
            CPMM_FEE_ID,
            fee_reserve,
            TOKEN_ID,
            native_token_account(WSOL_ID, owner, fee_reserve),
        )
    )


def self_test():
    assert not on_curve(b58decode(no_key()))
    assert on_curve(b58decode("ProCXqRcXJjoUd1RNoo28bSizAA6EEqt9wURZYPDc5u"))
    assert not on_curve(b58decode(CPMM_CONFIG_ID))
    config = amm_config(program_address([b"amm_config", b"\0\0"], CPMM_ID)[1], no_key())
    assert config[8 + 4 : 8 + 12] == struct.pack("<Q", TRADE_FEE_RATE)
    assert config[8 + 12 : 8 + 36] == bytes(24), "protocol, fund, and pool fees"
    assert config[8 + 36 : 8 + 100] == b58decode(no_key()) * 2
    assert config[8 + 100 :] == bytes(128), "creator fees and padding"
    for bad in ["", "0" * 32, "l" * 44, "1" * 45, '1111"\n  balance: 9']:
        try:
            b58decode(bad)
        except ValueError:
            continue
        raise AssertionError(f"{bad!r} decoded")
    text = accounts(4)
    assert text.count("\n") == 15 and '"' not in text.replace('data: "', "").replace(
        '"\n', "\n"
    )
    print("ok")


def main(argv):
    if argv[1:] == ["no-key"]:
        print(no_key())
    elif len(argv) == 4 and argv[1] == "accounts":
        lamports_per_byte_year = int(argv[2])
        if lamports_per_byte_year <= 0:
            raise ValueError("the lamports per byte year must be positive")
        with open(argv[3], "w", encoding="ascii") as out:
            out.write(accounts(lamports_per_byte_year))
    elif argv[1:] == ["self-test"]:
        self_test()
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
