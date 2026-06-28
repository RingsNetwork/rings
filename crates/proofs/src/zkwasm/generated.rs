#![allow(missing_docs)]
// This file is mechanically translated from Delphinus continuation-batcher Solidity verifier steps.
// The source Solidity verifier files declare SPDX-License-Identifier: MIT.
// Keep the handwritten verifier boundary in zkwasm/mod.rs; this module is generated glue.

use super::*;

pub(super) fn step1(transcript: &[Word], aux: &[Word], buf: &mut [Word]) -> Result<(), ProofError> {
    let value_10_11_a = word(transcript, 124)?;
    let value_10_11_b = word(transcript, 125)?;
    set_word(buf, 10, value_10_11_a)?;
    set_word(buf, 11, value_10_11_b)?;
    set_word(buf, 12, word_dec("1")?)?;
    ecc_mul(buf, 10)?;
    set_word(
        buf,
        17,
        evm_mulmod(
            word_dec(
                "13225785879531581993054172815365636627224369411478295502904397545373139154045",
            )?,
            word(buf, 6)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        18,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 17)?, q_mod_minus(word(buf, 6)?)?, q_mod()),
            word(aux, 0)?,
        )?,
    )?;
    set_word(
        buf,
        19,
        evm_mulmod(
            word_dec(
                "11211301017135681023579411905410872569206244553457844956874280139879520583390",
            )?,
            word(buf, 6)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 17)?, q_mod_minus(word(buf, 19)?)?, q_mod()),
            word(aux, 1)?,
        )?,
    )?;
    set_word(buf, 21, evm_mulmod(word(buf, 18)?, word(buf, 20)?, q_mod()))?;
    set_word(
        buf,
        22,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 6)?, q_mod_minus(word(buf, 17)?)?, q_mod()),
            word(aux, 2)?,
        )?,
    )?;
    set_word(
        buf,
        23,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 6)?, q_mod_minus(word(buf, 19)?)?, q_mod()),
            word(aux, 3)?,
        )?,
    )?;
    set_word(buf, 24, evm_mulmod(word(buf, 22)?, word(buf, 23)?, q_mod()))?;
    set_word(
        buf,
        25,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 19)?, q_mod_minus(word(buf, 17)?)?, q_mod()),
            word(aux, 4)?,
        )?,
    )?;
    set_word(
        buf,
        26,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 19)?, q_mod_minus(word(buf, 6)?)?, q_mod()),
            word(aux, 5)?,
        )?,
    )?;
    set_word(buf, 27, evm_mulmod(word(buf, 25)?, word(buf, 26)?, q_mod()))?;
    set_word(
        buf,
        28,
        q_mod_minus(evm_mulmod(word(buf, 18)?, word(buf, 6)?, q_mod()))?,
    )?;
    set_word(buf, 29, evm_mulmod(word(buf, 20)?, word(buf, 19)?, q_mod()))?;
    set_word(
        buf,
        18,
        evm_addmod(
            evm_mulmod(word(buf, 28)?, word(buf, 20)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 18)?, word(buf, 29)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        q_mod_minus(evm_mulmod(word(buf, 22)?, word(buf, 17)?, q_mod()))?,
    )?;
    set_word(buf, 30, evm_mulmod(word(buf, 23)?, word(buf, 19)?, q_mod()))?;
    set_word(
        buf,
        22,
        evm_addmod(
            evm_mulmod(word(buf, 20)?, word(buf, 23)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 22)?, word(buf, 30)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        31,
        q_mod_minus(evm_mulmod(word(buf, 25)?, word(buf, 17)?, q_mod()))?,
    )?;
    set_word(buf, 32, evm_mulmod(word(buf, 26)?, word(buf, 6)?, q_mod()))?;
    set_word(
        buf,
        25,
        evm_addmod(
            evm_mulmod(word(buf, 31)?, word(buf, 26)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 25)?, word(buf, 32)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 21)?, word(transcript, 101)?, q_mod()),
                        evm_mulmod(word(buf, 24)?, word(transcript, 99)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 27)?, word(transcript, 100)?, q_mod()),
                    q_mod(),
                ),
                word(buf, 9)?,
                q_mod(),
            ),
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(buf, 18)?, word(transcript, 101)?, q_mod()),
                    evm_mulmod(word(buf, 22)?, word(transcript, 99)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 25)?, word(transcript, 100)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        28,
        q_mod_minus(evm_mulmod(word(buf, 28)?, word(buf, 29)?, q_mod()))?,
    )?;
    set_word(
        buf,
        20,
        q_mod_minus(evm_mulmod(word(buf, 20)?, word(buf, 30)?, q_mod()))?,
    )?;
    set_word(
        buf,
        29,
        q_mod_minus(evm_mulmod(word(buf, 31)?, word(buf, 32)?, q_mod()))?,
    )?;
    set_word(
        buf,
        31,
        evm_mulmod(
            word(buf, 7)?,
            evm_addmod(
                evm_mulmod(word(buf, 33)?, word(buf, 9)?, q_mod()),
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 28)?, word(transcript, 101)?, q_mod()),
                        evm_mulmod(word(buf, 20)?, word(transcript, 99)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 29)?, word(transcript, 100)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 21)?, word(transcript, 104)?, q_mod()),
                        evm_mulmod(word(buf, 24)?, word(transcript, 102)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 27)?, word(transcript, 103)?, q_mod()),
                    q_mod(),
                ),
                word(buf, 9)?,
                q_mod(),
            ),
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(buf, 18)?, word(transcript, 104)?, q_mod()),
                    evm_mulmod(word(buf, 22)?, word(transcript, 102)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 25)?, word(transcript, 103)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        31,
        evm_addmod(
            word(buf, 31)?,
            evm_addmod(
                evm_mulmod(word(buf, 33)?, word(buf, 9)?, q_mod()),
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 28)?, word(transcript, 104)?, q_mod()),
                        evm_mulmod(word(buf, 20)?, word(transcript, 102)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 29)?, word(transcript, 103)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 21)?, word(transcript, 107)?, q_mod()),
                        evm_mulmod(word(buf, 24)?, word(transcript, 105)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 27)?, word(transcript, 106)?, q_mod()),
                    q_mod(),
                ),
                word(buf, 9)?,
                q_mod(),
            ),
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(buf, 18)?, word(transcript, 107)?, q_mod()),
                    evm_mulmod(word(buf, 22)?, word(transcript, 105)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 25)?, word(transcript, 106)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        31,
        evm_addmod(
            evm_mulmod(word(buf, 7)?, word(buf, 31)?, q_mod()),
            evm_addmod(
                evm_mulmod(word(buf, 33)?, word(buf, 9)?, q_mod()),
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 28)?, word(transcript, 107)?, q_mod()),
                        evm_mulmod(word(buf, 20)?, word(transcript, 105)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 29)?, word(transcript, 106)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 21)?, word(transcript, 110)?, q_mod()),
                        evm_mulmod(word(buf, 24)?, word(transcript, 108)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 27)?, word(transcript, 109)?, q_mod()),
                    q_mod(),
                ),
                word(buf, 9)?,
                q_mod(),
            ),
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(buf, 18)?, word(transcript, 110)?, q_mod()),
                    evm_mulmod(word(buf, 22)?, word(transcript, 108)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 25)?, word(transcript, 109)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        31,
        evm_addmod(
            evm_mulmod(word(buf, 7)?, word(buf, 31)?, q_mod()),
            evm_addmod(
                evm_mulmod(word(buf, 33)?, word(buf, 9)?, q_mod()),
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 28)?, word(transcript, 110)?, q_mod()),
                        evm_mulmod(word(buf, 20)?, word(transcript, 108)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 29)?, word(transcript, 109)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        18,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 21)?, word(transcript, 116)?, q_mod()),
                        evm_mulmod(word(buf, 24)?, word(transcript, 114)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 27)?, word(transcript, 115)?, q_mod()),
                    q_mod(),
                ),
                word(buf, 9)?,
                q_mod(),
            ),
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(buf, 18)?, word(transcript, 116)?, q_mod()),
                    evm_mulmod(word(buf, 22)?, word(transcript, 114)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 25)?, word(transcript, 115)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        18,
        evm_addmod(
            evm_mulmod(word(buf, 7)?, word(buf, 31)?, q_mod()),
            evm_addmod(
                evm_mulmod(word(buf, 18)?, word(buf, 9)?, q_mod()),
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 28)?, word(transcript, 116)?, q_mod()),
                        evm_mulmod(word(buf, 20)?, word(transcript, 114)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 29)?, word(transcript, 115)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_mulmod(
            word(buf, 7)?,
            evm_addmod(
                evm_mulmod(
                    word(buf, 7)?,
                    evm_addmod(
                        evm_mulmod(word(buf, 7)?, word(transcript, 46)?, q_mod()),
                        word(transcript, 53)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(transcript, 113)?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_addmod(
            evm_mulmod(
                word(buf, 7)?,
                evm_addmod(
                    evm_mulmod(
                        word(buf, 7)?,
                        evm_addmod(word(buf, 20)?, word(transcript, 119)?, q_mod()),
                        q_mod(),
                    ),
                    word(transcript, 72)?,
                    q_mod(),
                ),
                q_mod(),
            ),
            word(transcript, 73)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_mulmod(
            word(buf, 7)?,
            evm_addmod(
                evm_mulmod(
                    word(buf, 7)?,
                    evm_addmod(
                        evm_mulmod(word(buf, 7)?, word(buf, 20)?, q_mod()),
                        word(transcript, 74)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(transcript, 75)?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_addmod(
            evm_mulmod(
                word(buf, 7)?,
                evm_addmod(
                    evm_mulmod(
                        word(buf, 7)?,
                        evm_addmod(word(buf, 20)?, word(transcript, 76)?, q_mod()),
                        q_mod(),
                    ),
                    word(transcript, 77)?,
                    q_mod(),
                ),
                q_mod(),
            ),
            word(transcript, 78)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_mulmod(
            word(buf, 7)?,
            evm_addmod(
                evm_mulmod(
                    word(buf, 7)?,
                    evm_addmod(
                        evm_mulmod(word(buf, 7)?, word(buf, 20)?, q_mod()),
                        word(transcript, 79)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(transcript, 80)?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_addmod(
            evm_mulmod(
                word(buf, 7)?,
                evm_addmod(
                    evm_mulmod(
                        word(buf, 7)?,
                        evm_addmod(word(buf, 20)?, word(transcript, 81)?, q_mod()),
                        q_mod(),
                    ),
                    word(transcript, 82)?,
                    q_mod(),
                ),
                q_mod(),
            ),
            word(transcript, 83)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_mulmod(
            word(buf, 7)?,
            evm_addmod(
                evm_mulmod(
                    word(buf, 7)?,
                    evm_addmod(
                        evm_mulmod(word(buf, 7)?, word(buf, 20)?, q_mod()),
                        word(transcript, 84)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(transcript, 85)?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_addmod(
            evm_mulmod(
                word(buf, 7)?,
                evm_addmod(
                    evm_mulmod(
                        word(buf, 7)?,
                        evm_addmod(word(buf, 20)?, word(transcript, 86)?, q_mod()),
                        q_mod(),
                    ),
                    word(transcript, 87)?,
                    q_mod(),
                ),
                q_mod(),
            ),
            word(transcript, 89)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_mulmod(
            word(buf, 7)?,
            evm_addmod(
                evm_mulmod(
                    word(buf, 7)?,
                    evm_addmod(
                        evm_mulmod(word(buf, 7)?, word(buf, 20)?, q_mod()),
                        word(transcript, 90)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(transcript, 91)?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_addmod(
            evm_mulmod(
                word(buf, 7)?,
                evm_addmod(
                    evm_mulmod(
                        word(buf, 7)?,
                        evm_addmod(word(buf, 20)?, word(transcript, 92)?, q_mod()),
                        q_mod(),
                    ),
                    word(transcript, 93)?,
                    q_mod(),
                ),
                q_mod(),
            ),
            word(transcript, 94)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_mulmod(
            word(buf, 7)?,
            evm_addmod(
                evm_mulmod(
                    word(buf, 7)?,
                    evm_addmod(
                        evm_mulmod(word(buf, 7)?, word(buf, 20)?, q_mod()),
                        word(transcript, 95)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(transcript, 96)?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        21,
        evm_addmod(
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        word(transcript, 72)?,
                        evm_mulmod(word(transcript, 52)?, word(transcript, 73)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(transcript, 47)?, word(transcript, 74)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(transcript, 48)?, word(transcript, 75)?, q_mod()),
                q_mod(),
            ),
            evm_mulmod(word(transcript, 49)?, word(transcript, 76)?, q_mod()),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        22,
        evm_mulmod(word(transcript, 49)?, word(transcript, 50)?, q_mod()),
    )?;
    set_word(
        buf,
        21,
        evm_addmod(
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        word(buf, 21)?,
                        evm_mulmod(word(transcript, 50)?, word(transcript, 77)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(transcript, 51)?, word(transcript, 78)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(
                    evm_mulmod(word(transcript, 47)?, word(transcript, 48)?, q_mod()),
                    word(transcript, 79)?,
                    q_mod(),
                ),
                q_mod(),
            ),
            evm_mulmod(word(buf, 22)?, word(transcript, 80)?, q_mod()),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        24,
        evm_addmod(
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        word(transcript, 53)?,
                        q_mod_minus(word(transcript, 54)?)?,
                        q_mod(),
                    ),
                    q_mod_minus(evm_mulmod(
                        word(transcript, 55)?,
                        word_dec("262144")?,
                        q_mod(),
                    ))?,
                    q_mod(),
                ),
                q_mod_minus(evm_mulmod(
                    word(transcript, 56)?,
                    word_dec("68719476736")?,
                    q_mod(),
                ))?,
                q_mod(),
            ),
            q_mod_minus(evm_mulmod(
                word(transcript, 57)?,
                word_dec("18014398509481984")?,
                q_mod(),
            ))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        24,
        evm_mulmod(
            word(transcript, 82)?,
            evm_addmod(
                evm_addmod(
                    word(buf, 24)?,
                    q_mod_minus(evm_mulmod(
                        word(transcript, 58)?,
                        word_dec("4722366482869645213696")?,
                        q_mod(),
                    ))?,
                    q_mod(),
                ),
                q_mod_minus(evm_mulmod(
                    word(transcript, 59)?,
                    word_dec("1237940039285380274899124224")?,
                    q_mod(),
                ))?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        21,
        evm_mulmod(
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 21)?, word(buf, 5)?, q_mod()),
                        word(buf, 24)?,
                        q_mod(),
                    ),
                    word(buf, 5)?,
                    q_mod(),
                ),
                evm_mulmod(
                    word(transcript, 83)?,
                    evm_addmod(
                        evm_addmod(word(transcript, 54)?, word(transcript, 55)?, q_mod()),
                        q_mod_minus(word(transcript, 84)?)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                q_mod(),
            ),
            word(buf, 5)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        24,
        evm_addmod(
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(transcript, 47)?, word(transcript, 50)?, q_mod()),
                    q_mod_minus(evm_mulmod(
                        word(transcript, 60)?,
                        word_dec("214344503607247422601713164287303")?,
                        q_mod(),
                    ))?,
                    q_mod(),
                ),
                q_mod_minus(word(transcript, 52)?)?,
                q_mod(),
            ),
            word_dec("315936875005671560093754083051011945993792603055100900074974085120")?,
            q_mod(),
        ),
    )?;
    set_word(buf, 21, evm_addmod(word(buf, 21)?, evm_mulmod(evm_addmod(evm_addmod(evm_mulmod(word(transcript, 64)?, word_dec("105312291668557186697918027683670432318895095400549111254310977536")?, q_mod()), evm_mulmod(word(transcript, 65)?, word_dec("324518553658426726783156020576256")?, q_mod()), q_mod()), q_mod_minus(word(buf, 24)?)?, q_mod()), word(transcript, 85)?, q_mod()), q_mod()))?;
    set_word(
        buf,
        24,
        evm_addmod(
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(transcript, 47)?, word(transcript, 51)?, q_mod()),
                    q_mod_minus(evm_mulmod(
                        word(transcript, 60)?,
                        word_dec("62907968018002682745826825566230")?,
                        q_mod(),
                    ))?,
                    q_mod(),
                ),
                evm_addmod(
                    evm_mulmod(word(transcript, 48)?, word(transcript, 50)?, q_mod()),
                    q_mod_minus(evm_mulmod(
                        word(transcript, 61)?,
                        word_dec("214344503607247422601713164287303")?,
                        q_mod(),
                    ))?,
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod_minus(word(transcript, 66)?)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        24,
        q_mod_minus(evm_addmod(
            evm_addmod(
                evm_addmod(
                    word(buf, 24)?,
                    evm_mulmod(
                        word(transcript, 64)?,
                        word_dec("324518553658426726783156020576256")?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(transcript, 65)?,
                q_mod(),
            ),
            word_dec("315936875005671560093754083051010972438131627774920550606912356350")?,
            q_mod(),
        ))?,
    )?;
    set_word(buf, 21, evm_addmod(evm_mulmod(word(buf, 21)?, word(buf, 5)?, q_mod()), evm_mulmod(evm_addmod(evm_addmod(evm_mulmod(word(transcript, 67)?, word_dec("105312291668557186697918027683670432318895095400549111254310977536")?, q_mod()), evm_mulmod(word(transcript, 68)?, word_dec("324518553658426726783156020576256")?, q_mod()), q_mod()), word(buf, 24)?, q_mod()), word(transcript, 85)?, q_mod()), q_mod()))?;
    set_word(
        buf,
        22,
        evm_addmod(
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(transcript, 47)?, word(transcript, 62)?, q_mod()),
                    q_mod_minus(evm_mulmod(
                        word(transcript, 60)?,
                        word_dec("207841293025")?,
                        q_mod(),
                    ))?,
                    q_mod(),
                ),
                evm_addmod(
                    evm_mulmod(word(transcript, 48)?, word(transcript, 51)?, q_mod()),
                    q_mod_minus(evm_mulmod(
                        word(transcript, 61)?,
                        word_dec("62907968018002682745826825566230")?,
                        q_mod(),
                    ))?,
                    q_mod(),
                ),
                q_mod(),
            ),
            evm_addmod(
                word(buf, 22)?,
                q_mod_minus(evm_mulmod(
                    word(transcript, 63)?,
                    word_dec("214344503607247422601713164287303")?,
                    q_mod(),
                ))?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        22,
        evm_addmod(
            evm_addmod(
                evm_addmod(
                    evm_addmod(word(buf, 22)?, q_mod_minus(word(transcript, 69)?)?, q_mod()),
                    evm_mulmod(
                        word(transcript, 67)?,
                        word_dec("324518553658426726783156020576256")?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(transcript, 68)?,
                q_mod(),
            ),
            word_dec("315936875005671560093754083051010972438131627774920550606912356350")?,
            q_mod(),
        ),
    )?;
    set_word(buf, 21, evm_addmod(evm_mulmod(word(buf, 21)?, word(buf, 5)?, q_mod()), evm_mulmod(evm_addmod(evm_addmod(evm_mulmod(word(transcript, 70)?, word_dec("105312291668557186697918027683670432318895095400549111254310977536")?, q_mod()), evm_mulmod(word(transcript, 71)?, word_dec("324518553658426726783156020576256")?, q_mod()), q_mod()), q_mod_minus(word(buf, 22)?)?, q_mod()), word(transcript, 85)?, q_mod()), q_mod()))?;
    set_word(buf, 22, fr_pow(word(buf, 6)?, word_dec("4194304")?))?;
    set_word(
        buf,
        24,
        evm_addmod(word(buf, 22)?, q_mod_minus(word_dec("1")?)?, q_mod()),
    )?;
    set_word(
        buf,
        25,
        fr_div(
            evm_mulmod(
                word_dec(
                    "21888237653275510688422624196183639687472264873923820041627027729598873448513",
                )?,
                word(buf, 24)?,
                q_mod(),
            ),
            evm_addmod(word(buf, 6)?, q_mod_minus(word_dec("1")?)?, q_mod()),
            word(aux, 6)?,
        )?,
    )?;
    set_word(
        buf,
        27,
        fr_div(
            evm_mulmod(
                word_dec(
                    "5743945824801343390185369419535128265315198471736271400343304585804193827880",
                )?,
                word(buf, 24)?,
                q_mod(),
            ),
            evm_addmod(
                word(buf, 6)?,
                q_mod_minus(word_dec(
                    "13225785879531581993054172815365636627224369411478295502904397545373139154045",
                )?)?,
                q_mod(),
            ),
            word(aux, 7)?,
        )?,
    )?;
    set_word(
        buf,
        21,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_mulmod(word(buf, 21)?, word(buf, 5)?, q_mod()),
                    evm_mulmod(
                        word(buf, 25)?,
                        evm_addmod(word_dec("1")?, q_mod_minus(word(transcript, 99)?)?, q_mod()),
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(buf, 5)?,
                q_mod(),
            ),
            evm_mulmod(
                word(buf, 27)?,
                evm_addmod(
                    evm_mulmod(word(transcript, 111)?, word(transcript, 111)?, q_mod()),
                    q_mod_minus(word(transcript, 111)?)?,
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        21,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_mulmod(word(buf, 21)?, word(buf, 5)?, q_mod()),
                    evm_mulmod(
                        evm_addmod(
                            word(transcript, 102)?,
                            q_mod_minus(word(transcript, 101)?)?,
                            q_mod(),
                        ),
                        word(buf, 25)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(buf, 5)?,
                q_mod(),
            ),
            evm_mulmod(
                evm_addmod(
                    word(transcript, 105)?,
                    q_mod_minus(word(transcript, 104)?)?,
                    q_mod(),
                ),
                word(buf, 25)?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        21,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_mulmod(word(buf, 21)?, word(buf, 5)?, q_mod()),
                    evm_mulmod(
                        evm_addmod(
                            word(transcript, 108)?,
                            q_mod_minus(word(transcript, 107)?)?,
                            q_mod(),
                        ),
                        word(buf, 25)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(buf, 5)?,
                q_mod(),
            ),
            evm_mulmod(
                evm_addmod(
                    word(transcript, 111)?,
                    q_mod_minus(word(transcript, 110)?)?,
                    q_mod(),
                ),
                word(buf, 25)?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        28,
        evm_addmod(word(transcript, 47)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(
        buf,
        29,
        evm_addmod(word(transcript, 46)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(buf, 31, evm_mulmod(word(buf, 3)?, word(buf, 6)?, q_mod()))?;
    set_word(buf, 28, evm_addmod(evm_mulmod(evm_addmod(word(buf, 28)?, evm_mulmod(word(buf, 3)?, word(transcript, 90)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 29)?, evm_mulmod(word(buf, 3)?, word(transcript, 89)?, q_mod()), q_mod()), word(transcript, 100)?, q_mod()), q_mod()), q_mod_minus(evm_mulmod(evm_addmod(word(buf, 28)?, evm_mulmod(word_dec("4131629893567559867359510883348571134090853742863529169391034518566172092834")?, word(buf, 31)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 29)?, word(buf, 31)?, q_mod()), word(transcript, 99)?, q_mod()), q_mod()))?, q_mod()))?;
    set_word(buf, 29, evm_addmod(evm_addmod(evm_addmod(fr_div(evm_mulmod(word_dec("20023042075029862075635603136649050502962424708267292886390647475108663608857")?, word(buf, 24)?, q_mod()), evm_addmod(word(buf, 6)?, q_mod_minus(word_dec("10939663269433627367777756708678102241564365262857670666700619874077960926249")?)?, q_mod()), word(aux, 8)?)?, fr_div(evm_mulmod(word_dec("496209762031177553439375370250532367801224970379575774747024844773905018536")?, word(buf, 24)?, q_mod()), evm_addmod(word(buf, 6)?, q_mod_minus(word_dec("11016257578652593686382655500910603527869149377564754001549454008164059876499")?)?, q_mod()), word(aux, 9)?)?, q_mod()), fr_div(evm_mulmod(word_dec("20459617746544248062014976317203465365908990827508925305769002868034509119086")?, word(buf, 24)?, q_mod()), evm_addmod(word(buf, 6)?, q_mod_minus(word_dec("15402826414547299628414612080036060696555554914079673875872749760617770134879")?)?, q_mod()), word(aux, 10)?)?, q_mod()), fr_div(evm_mulmod(word_dec("9952375098572582562392692839581731570430874250722926349774599560449354965478")?, word(buf, 24)?, q_mod()), evm_addmod(word(buf, 6)?, q_mod_minus(word_dec("21710372849001950800533397158415938114909991150039389063546734567764856596059")?)?, q_mod()), word(aux, 11)?)?, q_mod()))?;
    set_word(buf, 29, evm_addmod(word(buf, 27)?, evm_addmod(evm_addmod(word(buf, 29)?, fr_div(evm_mulmod(word_dec("2475562068482919789434538161456555368473369493180072113639899532770322825977")?, word(buf, 24)?, q_mod()), evm_addmod(word(buf, 6)?, q_mod_minus(word_dec("2785514556381676080176937710880804108647911392478702105860685610379369825016")?)?, q_mod()), word(aux, 12)?)?, q_mod()), fr_div(evm_mulmod(word_dec("12919475148704033459056799975164749366765443418491560826543287262494049147445")?, word(buf, 24)?, q_mod()), evm_addmod(word(buf, 6)?, q_mod_minus(word_dec("8734126352828345679573237859165904705806588461301144420590422589042130041188")?)?, q_mod()), word(aux, 13)?)?, q_mod()), q_mod()))?;
    set_word(
        buf,
        29,
        evm_addmod(word_dec("1")?, q_mod_minus(word(buf, 29)?)?, q_mod()),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(word(transcript, 49)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(
        buf,
        34,
        evm_addmod(word(transcript, 48)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(
        buf,
        35,
        fr_pow(
            word_dec(
                "4131629893567559867359510883348571134090853742863529169391034518566172092834",
            )?,
            word_dec("2")?,
        ),
    )?;
    set_word(buf, 35, evm_mulmod(word(buf, 31)?, word(buf, 35)?, q_mod()))?;
    set_word(buf, 33, evm_addmod(evm_mulmod(evm_addmod(word(buf, 33)?, evm_mulmod(word(buf, 3)?, word(transcript, 92)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 34)?, evm_mulmod(word(buf, 3)?, word(transcript, 91)?, q_mod()), q_mod()), word(transcript, 103)?, q_mod()), q_mod()), q_mod_minus(evm_mulmod(evm_addmod(word(buf, 33)?, evm_mulmod(word_dec("4131629893567559867359510883348571134090853742863529169391034518566172092834")?, word(buf, 35)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 34)?, word(buf, 35)?, q_mod()), word(transcript, 102)?, q_mod()), q_mod()))?, q_mod()))?;
    set_word(
        buf,
        21,
        evm_mulmod(
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 21)?, word(buf, 5)?, q_mod()),
                        evm_mulmod(word(buf, 28)?, word(buf, 29)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 5)?,
                    q_mod(),
                ),
                evm_mulmod(word(buf, 33)?, word(buf, 29)?, q_mod()),
                q_mod(),
            ),
            word(buf, 5)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        28,
        evm_addmod(word(transcript, 51)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(word(transcript, 50)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(
        buf,
        34,
        fr_pow(
            word_dec(
                "4131629893567559867359510883348571134090853742863529169391034518566172092834",
            )?,
            word_dec("4")?,
        ),
    )?;
    set_word(buf, 34, evm_mulmod(word(buf, 31)?, word(buf, 34)?, q_mod()))?;
    set_word(buf, 28, evm_addmod(evm_mulmod(evm_addmod(word(buf, 28)?, evm_mulmod(word(buf, 3)?, word(transcript, 94)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 33)?, evm_mulmod(word(buf, 3)?, word(transcript, 93)?, q_mod()), q_mod()), word(transcript, 106)?, q_mod()), q_mod()), q_mod_minus(evm_mulmod(evm_addmod(word(buf, 28)?, evm_mulmod(word_dec("4131629893567559867359510883348571134090853742863529169391034518566172092834")?, word(buf, 34)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 33)?, word(buf, 34)?, q_mod()), word(transcript, 105)?, q_mod()), q_mod()))?, q_mod()))?;
    set_word(
        buf,
        33,
        evm_addmod(word(transcript, 54)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(
        buf,
        34,
        evm_addmod(word(transcript, 53)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(
        buf,
        35,
        fr_pow(
            word_dec(
                "4131629893567559867359510883348571134090853742863529169391034518566172092834",
            )?,
            word_dec("6")?,
        ),
    )?;
    set_word(buf, 35, evm_mulmod(word(buf, 31)?, word(buf, 35)?, q_mod()))?;
    Ok(())
}

pub(super) fn step2(transcript: &[Word], aux: &[Word], buf: &mut [Word]) -> Result<(), ProofError> {
    set_word(buf, 33, evm_addmod(evm_mulmod(evm_addmod(word(buf, 33)?, evm_mulmod(word(buf, 3)?, word(transcript, 96)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 34)?, evm_mulmod(word(buf, 3)?, word(transcript, 95)?, q_mod()), q_mod()), word(transcript, 109)?, q_mod()), q_mod()), q_mod_minus(evm_mulmod(evm_addmod(word(buf, 33)?, evm_mulmod(word_dec("4131629893567559867359510883348571134090853742863529169391034518566172092834")?, word(buf, 35)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 34)?, word(buf, 35)?, q_mod()), word(transcript, 108)?, q_mod()), q_mod()))?, q_mod()))?;
    set_word(
        buf,
        21,
        evm_mulmod(
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        word(buf, 21)?,
                        evm_mulmod(word(buf, 28)?, word(buf, 29)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 5)?,
                    q_mod(),
                ),
                evm_mulmod(word(buf, 33)?, word(buf, 29)?, q_mod()),
                q_mod(),
            ),
            word(buf, 5)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        28,
        evm_addmod(word(transcript, 56)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(word(transcript, 55)?, word(buf, 4)?, q_mod()),
    )?;
    set_word(
        buf,
        34,
        fr_pow(
            word_dec(
                "4131629893567559867359510883348571134090853742863529169391034518566172092834",
            )?,
            word_dec("8")?,
        ),
    )?;
    set_word(buf, 31, evm_mulmod(word(buf, 31)?, word(buf, 34)?, q_mod()))?;
    set_word(buf, 28, evm_addmod(evm_mulmod(evm_addmod(word(buf, 28)?, evm_mulmod(word(buf, 3)?, word(transcript, 98)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 33)?, evm_mulmod(word(buf, 3)?, word(transcript, 97)?, q_mod()), q_mod()), word(transcript, 112)?, q_mod()), q_mod()), q_mod_minus(evm_mulmod(evm_addmod(word(buf, 28)?, evm_mulmod(word_dec("4131629893567559867359510883348571134090853742863529169391034518566172092834")?, word(buf, 31)?, q_mod()), q_mod()), evm_mulmod(evm_addmod(word(buf, 33)?, word(buf, 31)?, q_mod()), word(transcript, 111)?, q_mod()), q_mod()))?, q_mod()))?;
    set_word(
        buf,
        21,
        evm_mulmod(
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        word(buf, 21)?,
                        evm_mulmod(word(buf, 28)?, word(buf, 29)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 5)?,
                    q_mod(),
                ),
                evm_mulmod(word(buf, 25)?, word(transcript, 114)?, q_mod()),
                q_mod(),
            ),
            word(buf, 5)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        28,
        evm_addmod(word(transcript, 81)?, word(buf, 3)?, q_mod()),
    )?;
    set_word(
        buf,
        28,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_mulmod(
                        word(buf, 28)?,
                        evm_addmod(
                            word(transcript, 115)?,
                            q_mod_minus(word(transcript, 114)?)?,
                            q_mod(),
                        ),
                        q_mod(),
                    ),
                    word(transcript, 113)?,
                    q_mod(),
                ),
                evm_addmod(word(transcript, 54)?, word(buf, 3)?, q_mod()),
                q_mod(),
            ),
            q_mod_minus(word(buf, 28)?)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        21,
        evm_mulmod(
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        word(buf, 21)?,
                        evm_mulmod(word(buf, 27)?, word(transcript, 117)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 5)?,
                    q_mod(),
                ),
                evm_mulmod(word(buf, 28)?, word(buf, 29)?, q_mod()),
                q_mod(),
            ),
            word(buf, 5)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        28,
        evm_addmod(word(transcript, 55)?, word(buf, 3)?, q_mod()),
    )?;
    set_word(
        buf,
        31,
        evm_addmod(word(transcript, 56)?, word(buf, 3)?, q_mod()),
    )?;
    set_word(
        buf,
        21,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    word(buf, 21)?,
                    evm_mulmod(
                        word(buf, 25)?,
                        evm_addmod(
                            word(transcript, 117)?,
                            q_mod_minus(word(transcript, 116)?)?,
                            q_mod(),
                        ),
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(buf, 5)?,
                q_mod(),
            ),
            evm_mulmod(
                evm_addmod(
                    evm_mulmod(
                        evm_addmod(
                            word(transcript, 118)?,
                            q_mod_minus(word(transcript, 117)?)?,
                            q_mod(),
                        ),
                        evm_mulmod(word(buf, 28)?, word(buf, 31)?, q_mod()),
                        q_mod(),
                    ),
                    q_mod_minus(evm_addmod(word(buf, 31)?, word(buf, 28)?, q_mod()))?,
                    q_mod(),
                ),
                word(buf, 29)?,
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        21,
        evm_mulmod(
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 21)?, word(buf, 5)?, q_mod()),
                        evm_mulmod(word(buf, 25)?, word(transcript, 120)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 5)?,
                    q_mod(),
                ),
                evm_mulmod(word(buf, 27)?, word(transcript, 120)?, q_mod()),
                q_mod(),
            ),
            word(buf, 5)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        25,
        evm_mulmod(
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(transcript, 86)?, word(buf, 2)?, q_mod()),
                        word(transcript, 47)?,
                        q_mod(),
                    ),
                    word(buf, 2)?,
                    q_mod(),
                ),
                word(transcript, 48)?,
                q_mod(),
            ),
            word(buf, 2)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        27,
        evm_addmod(
            evm_addmod(word(buf, 25)?, word(transcript, 87)?, q_mod()),
            word(buf, 3)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        25,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_mulmod(
                        word(buf, 27)?,
                        evm_addmod(
                            word(transcript, 121)?,
                            q_mod_minus(word(transcript, 120)?)?,
                            q_mod(),
                        ),
                        q_mod(),
                    ),
                    word(transcript, 119)?,
                    q_mod(),
                ),
                evm_addmod(word(buf, 25)?, word(buf, 3)?, q_mod()),
                q_mod(),
            ),
            q_mod_minus(word(buf, 27)?)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        evm_addmod(
            evm_mulmod(
                word(buf, 7)?,
                evm_addmod(
                    evm_mulmod(
                        word(buf, 7)?,
                        evm_addmod(word(buf, 20)?, word(transcript, 97)?, q_mod()),
                        q_mod(),
                    ),
                    word(transcript, 98)?,
                    q_mod(),
                ),
                q_mod(),
            ),
            fr_div(
                evm_addmod(
                    word(buf, 21)?,
                    evm_mulmod(word(buf, 25)?, word(buf, 29)?, q_mod()),
                    q_mod(),
                ),
                word(buf, 24)?,
                word(aux, 14)?,
            )?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        17,
        evm_addmod(word(buf, 9)?, q_mod_minus(word(buf, 17)?)?, q_mod()),
    )?;
    set_word(
        buf,
        21,
        evm_addmod(word(buf, 9)?, q_mod_minus(word(buf, 19)?)?, q_mod()),
    )?;
    set_word(
        buf,
        24,
        evm_mulmod(
            word_dec(
                "1426404432721484388505361748317961535523355871255605456897797744433766488507",
            )?,
            word(buf, 6)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        25,
        evm_addmod(word(buf, 9)?, q_mod_minus(word(buf, 24)?)?, q_mod()),
    )?;
    set_word(
        buf,
        27,
        evm_mulmod(
            word_dec(
                "12619617507853212586156872920672483948819476989779550311307282715684870266992",
            )?,
            word(buf, 6)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        28,
        evm_addmod(word(buf, 9)?, q_mod_minus(word(buf, 27)?)?, q_mod()),
    )?;
    set_word(
        buf,
        29,
        fr_div(
            word_dec("1")?,
            evm_mulmod(word(buf, 25)?, word(buf, 28)?, q_mod()),
            word(aux, 15)?,
        )?,
    )?;
    set_word(
        buf,
        31,
        evm_mulmod(
            evm_mulmod(
                evm_mulmod(
                    evm_mulmod(word(buf, 17)?, word(buf, 21)?, q_mod()),
                    word(buf, 25)?,
                    q_mod(),
                ),
                word(buf, 28)?,
                q_mod(),
            ),
            word(buf, 29)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        18,
        evm_mulmod(
            word(buf, 8)?,
            evm_addmod(
                evm_mulmod(word(buf, 8)?, word(buf, 18)?, q_mod()),
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 7)?, word(buf, 20)?, q_mod()),
                        word(transcript, 88)?,
                        q_mod(),
                    ),
                    word(buf, 31)?,
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(buf, 20, q_mod_minus(word(buf, 30)?)?)?;
    set_word(buf, 30, q_mod_minus(word(buf, 32)?)?)?;
    set_word(
        buf,
        32,
        evm_mulmod(
            word(buf, 7)?,
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 23)?, word(transcript, 54)?, q_mod()),
                        evm_mulmod(word(buf, 26)?, word(transcript, 57)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 9)?,
                    q_mod(),
                ),
                evm_addmod(
                    evm_mulmod(word(buf, 20)?, word(transcript, 54)?, q_mod()),
                    evm_mulmod(word(buf, 30)?, word(transcript, 57)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        32,
        evm_addmod(
            word(buf, 32)?,
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 23)?, word(transcript, 55)?, q_mod()),
                        evm_mulmod(word(buf, 26)?, word(transcript, 58)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 9)?,
                    q_mod(),
                ),
                evm_addmod(
                    evm_mulmod(word(buf, 20)?, word(transcript, 55)?, q_mod()),
                    evm_mulmod(word(buf, 30)?, word(transcript, 58)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        32,
        evm_addmod(
            evm_mulmod(word(buf, 7)?, word(buf, 32)?, q_mod()),
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 23)?, word(transcript, 56)?, q_mod()),
                        evm_mulmod(word(buf, 26)?, word(transcript, 59)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 9)?,
                    q_mod(),
                ),
                evm_addmod(
                    evm_mulmod(word(buf, 20)?, word(transcript, 56)?, q_mod()),
                    evm_mulmod(word(buf, 30)?, word(transcript, 59)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        32,
        evm_addmod(
            evm_mulmod(word(buf, 7)?, word(buf, 32)?, q_mod()),
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 23)?, word(transcript, 111)?, q_mod()),
                        evm_mulmod(word(buf, 26)?, word(transcript, 112)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 9)?,
                    q_mod(),
                ),
                evm_addmod(
                    evm_mulmod(word(buf, 20)?, word(transcript, 111)?, q_mod()),
                    evm_mulmod(word(buf, 30)?, word(transcript, 112)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        32,
        evm_addmod(
            evm_mulmod(word(buf, 7)?, word(buf, 32)?, q_mod()),
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 23)?, word(transcript, 117)?, q_mod()),
                        evm_mulmod(word(buf, 26)?, word(transcript, 118)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 9)?,
                    q_mod(),
                ),
                evm_addmod(
                    evm_mulmod(word(buf, 20)?, word(transcript, 117)?, q_mod()),
                    evm_mulmod(word(buf, 30)?, word(transcript, 118)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        32,
        evm_addmod(
            evm_mulmod(word(buf, 7)?, word(buf, 32)?, q_mod()),
            evm_addmod(
                evm_mulmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 23)?, word(transcript, 120)?, q_mod()),
                        evm_mulmod(word(buf, 26)?, word(transcript, 121)?, q_mod()),
                        q_mod(),
                    ),
                    word(buf, 9)?,
                    q_mod(),
                ),
                evm_addmod(
                    evm_mulmod(word(buf, 20)?, word(transcript, 120)?, q_mod()),
                    evm_mulmod(word(buf, 30)?, word(transcript, 121)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        25,
        evm_mulmod(
            evm_mulmod(
                evm_mulmod(word(buf, 17)?, word(buf, 25)?, q_mod()),
                word(buf, 28)?,
                q_mod(),
            ),
            word(buf, 29)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        33,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 6)?, q_mod_minus(word(buf, 24)?)?, q_mod()),
            word(aux, 16)?,
        )?,
    )?;
    set_word(buf, 34, evm_mulmod(word(buf, 23)?, word(buf, 33)?, q_mod()))?;
    set_word(
        buf,
        35,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 19)?, q_mod_minus(word(buf, 24)?)?, q_mod()),
            word(aux, 17)?,
        )?,
    )?;
    set_word(buf, 36, evm_mulmod(word(buf, 26)?, word(buf, 35)?, q_mod()))?;
    set_word(
        buf,
        37,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 24)?, q_mod_minus(word(buf, 6)?)?, q_mod()),
            word(aux, 18)?,
        )?,
    )?;
    set_word(
        buf,
        38,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 24)?, q_mod_minus(word(buf, 19)?)?, q_mod()),
            word(aux, 19)?,
        )?,
    )?;
    set_word(buf, 39, evm_mulmod(word(buf, 37)?, word(buf, 38)?, q_mod()))?;
    set_word(buf, 40, evm_mulmod(word(buf, 33)?, word(buf, 24)?, q_mod()))?;
    set_word(
        buf,
        23,
        evm_addmod(
            evm_mulmod(word(buf, 20)?, word(buf, 33)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 23)?, word(buf, 40)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(buf, 33, evm_mulmod(word(buf, 35)?, word(buf, 24)?, q_mod()))?;
    set_word(
        buf,
        26,
        evm_addmod(
            evm_mulmod(word(buf, 30)?, word(buf, 35)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 26)?, word(buf, 33)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        35,
        q_mod_minus(evm_mulmod(word(buf, 37)?, word(buf, 6)?, q_mod()))?,
    )?;
    set_word(buf, 41, evm_mulmod(word(buf, 38)?, word(buf, 19)?, q_mod()))?;
    set_word(
        buf,
        37,
        evm_addmod(
            evm_mulmod(word(buf, 35)?, word(buf, 38)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 37)?, word(buf, 41)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        38,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 34)?, word(transcript, 50)?, q_mod()),
                        evm_mulmod(word(buf, 36)?, word(transcript, 63)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 39)?, word(transcript, 65)?, q_mod()),
                    q_mod(),
                ),
                word(buf, 9)?,
                q_mod(),
            ),
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(buf, 23)?, word(transcript, 50)?, q_mod()),
                    evm_mulmod(word(buf, 26)?, word(transcript, 63)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 37)?, word(transcript, 65)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        q_mod_minus(evm_mulmod(word(buf, 20)?, word(buf, 40)?, q_mod()))?,
    )?;
    set_word(
        buf,
        30,
        q_mod_minus(evm_mulmod(word(buf, 30)?, word(buf, 33)?, q_mod()))?,
    )?;
    set_word(
        buf,
        33,
        q_mod_minus(evm_mulmod(word(buf, 35)?, word(buf, 41)?, q_mod()))?,
    )?;
    set_word(
        buf,
        35,
        evm_mulmod(
            word(buf, 7)?,
            evm_addmod(
                evm_mulmod(word(buf, 38)?, word(buf, 9)?, q_mod()),
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 20)?, word(transcript, 50)?, q_mod()),
                        evm_mulmod(word(buf, 30)?, word(transcript, 63)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 33)?, word(transcript, 65)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        38,
        evm_addmod(
            evm_mulmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 34)?, word(transcript, 51)?, q_mod()),
                        evm_mulmod(word(buf, 36)?, word(transcript, 52)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 39)?, word(transcript, 67)?, q_mod()),
                    q_mod(),
                ),
                word(buf, 9)?,
                q_mod(),
            ),
            evm_addmod(
                evm_addmod(
                    evm_mulmod(word(buf, 23)?, word(transcript, 51)?, q_mod()),
                    evm_mulmod(word(buf, 26)?, word(transcript, 52)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 37)?, word(transcript, 67)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        35,
        evm_addmod(
            word(buf, 35)?,
            evm_addmod(
                evm_mulmod(word(buf, 38)?, word(buf, 9)?, q_mod()),
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 20)?, word(transcript, 51)?, q_mod()),
                        evm_mulmod(word(buf, 30)?, word(transcript, 52)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 33)?, word(transcript, 67)?, q_mod()),
                    q_mod(),
                ),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        28,
        evm_mulmod(
            evm_mulmod(word(buf, 17)?, word(buf, 28)?, q_mod()),
            word(buf, 29)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        18,
        evm_mulmod(
            word(buf, 8)?,
            evm_addmod(
                evm_mulmod(
                    word(buf, 8)?,
                    evm_addmod(
                        word(buf, 18)?,
                        evm_mulmod(word(buf, 32)?, word(buf, 25)?, q_mod()),
                        q_mod(),
                    ),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 35)?, word(buf, 28)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        32,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 6)?, q_mod_minus(word(buf, 27)?)?, q_mod()),
            word(aux, 20)?,
        )?,
    )?;
    set_word(buf, 35, evm_mulmod(word(buf, 34)?, word(buf, 32)?, q_mod()))?;
    set_word(
        buf,
        38,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 19)?, q_mod_minus(word(buf, 27)?)?, q_mod()),
            word(aux, 21)?,
        )?,
    )?;
    set_word(buf, 40, evm_mulmod(word(buf, 36)?, word(buf, 38)?, q_mod()))?;
    set_word(
        buf,
        41,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 24)?, q_mod_minus(word(buf, 27)?)?, q_mod()),
            word(aux, 22)?,
        )?,
    )?;
    set_word(buf, 42, evm_mulmod(word(buf, 39)?, word(buf, 41)?, q_mod()))?;
    set_word(
        buf,
        43,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 27)?, q_mod_minus(word(buf, 6)?)?, q_mod()),
            word(aux, 23)?,
        )?,
    )?;
    set_word(
        buf,
        44,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 27)?, q_mod_minus(word(buf, 19)?)?, q_mod()),
            word(aux, 24)?,
        )?,
    )?;
    set_word(buf, 45, evm_mulmod(word(buf, 43)?, word(buf, 44)?, q_mod()))?;
    set_word(
        buf,
        46,
        fr_div(
            word_dec("1")?,
            evm_addmod(word(buf, 27)?, q_mod_minus(word(buf, 24)?)?, q_mod()),
            word(aux, 25)?,
        )?,
    )?;
    set_word(buf, 47, evm_mulmod(word(buf, 45)?, word(buf, 46)?, q_mod()))?;
    set_word(
        buf,
        48,
        evm_mulmod(
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 35)?, word(transcript, 47)?, q_mod()),
                        evm_mulmod(word(buf, 40)?, word(transcript, 62)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 42)?, word(transcript, 66)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 47)?, word(transcript, 68)?, q_mod()),
                q_mod(),
            ),
            word(buf, 9)?,
            q_mod(),
        ),
    )?;
    set_word(buf, 49, evm_mulmod(word(buf, 32)?, word(buf, 27)?, q_mod()))?;
    set_word(
        buf,
        34,
        evm_addmod(
            evm_mulmod(word(buf, 23)?, word(buf, 32)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 34)?, word(buf, 49)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(buf, 50, evm_mulmod(word(buf, 38)?, word(buf, 27)?, q_mod()))?;
    set_word(
        buf,
        36,
        evm_addmod(
            evm_mulmod(word(buf, 26)?, word(buf, 38)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 36)?, word(buf, 50)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(buf, 27, evm_mulmod(word(buf, 41)?, word(buf, 27)?, q_mod()))?;
    set_word(
        buf,
        39,
        evm_addmod(
            evm_mulmod(word(buf, 37)?, word(buf, 41)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 39)?, word(buf, 27)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        51,
        q_mod_minus(evm_mulmod(word(buf, 43)?, word(buf, 6)?, q_mod()))?,
    )?;
    set_word(buf, 19, evm_mulmod(word(buf, 44)?, word(buf, 19)?, q_mod()))?;
    set_word(
        buf,
        43,
        evm_addmod(
            evm_mulmod(word(buf, 51)?, word(buf, 44)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 43)?, word(buf, 19)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(buf, 24, evm_mulmod(word(buf, 46)?, word(buf, 24)?, q_mod()))?;
    set_word(
        buf,
        44,
        evm_addmod(
            evm_mulmod(word(buf, 43)?, word(buf, 46)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 45)?, word(buf, 24)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        45,
        evm_addmod(
            word(buf, 48)?,
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 34)?, word(transcript, 47)?, q_mod()),
                        evm_mulmod(word(buf, 36)?, word(transcript, 62)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 39)?, word(transcript, 66)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 44)?, word(transcript, 68)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        23,
        evm_addmod(
            evm_mulmod(word(buf, 20)?, word(buf, 32)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 23)?, word(buf, 49)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        26,
        evm_addmod(
            evm_mulmod(word(buf, 30)?, word(buf, 38)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 26)?, word(buf, 50)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        32,
        evm_addmod(
            evm_mulmod(word(buf, 33)?, word(buf, 41)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 37)?, word(buf, 27)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        19,
        q_mod_minus(evm_mulmod(word(buf, 51)?, word(buf, 19)?, q_mod()))?,
    )?;
    set_word(
        buf,
        37,
        evm_addmod(
            evm_mulmod(word(buf, 19)?, word(buf, 46)?, q_mod()),
            q_mod_minus(evm_mulmod(word(buf, 43)?, word(buf, 24)?, q_mod()))?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        38,
        evm_addmod(
            evm_mulmod(word(buf, 45)?, word(buf, 9)?, q_mod()),
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 23)?, word(transcript, 47)?, q_mod()),
                        evm_mulmod(word(buf, 26)?, word(transcript, 62)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 32)?, word(transcript, 66)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 37)?, word(transcript, 68)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        20,
        q_mod_minus(evm_mulmod(word(buf, 20)?, word(buf, 49)?, q_mod()))?,
    )?;
    set_word(
        buf,
        30,
        q_mod_minus(evm_mulmod(word(buf, 30)?, word(buf, 50)?, q_mod()))?,
    )?;
    set_word(
        buf,
        27,
        q_mod_minus(evm_mulmod(word(buf, 33)?, word(buf, 27)?, q_mod()))?,
    )?;
    set_word(
        buf,
        19,
        q_mod_minus(evm_mulmod(word(buf, 19)?, word(buf, 24)?, q_mod()))?,
    )?;
    set_word(
        buf,
        24,
        evm_addmod(
            evm_mulmod(word(buf, 38)?, word(buf, 9)?, q_mod()),
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 20)?, word(transcript, 47)?, q_mod()),
                        evm_mulmod(word(buf, 30)?, word(transcript, 62)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 27)?, word(transcript, 66)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 19)?, word(transcript, 68)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        33,
        evm_mulmod(
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 35)?, word(transcript, 48)?, q_mod()),
                        evm_mulmod(word(buf, 40)?, word(transcript, 60)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 42)?, word(transcript, 69)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 47)?, word(transcript, 70)?, q_mod()),
                q_mod(),
            ),
            word(buf, 9)?,
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(
            word(buf, 33)?,
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 34)?, word(transcript, 48)?, q_mod()),
                        evm_mulmod(word(buf, 36)?, word(transcript, 60)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 39)?, word(transcript, 69)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 44)?, word(transcript, 70)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(
            evm_mulmod(word(buf, 33)?, word(buf, 9)?, q_mod()),
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 23)?, word(transcript, 48)?, q_mod()),
                        evm_mulmod(word(buf, 26)?, word(transcript, 60)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 32)?, word(transcript, 69)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 37)?, word(transcript, 70)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        33,
        evm_addmod(
            evm_mulmod(word(buf, 33)?, word(buf, 9)?, q_mod()),
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 20)?, word(transcript, 48)?, q_mod()),
                        evm_mulmod(word(buf, 30)?, word(transcript, 60)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 27)?, word(transcript, 69)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 19)?, word(transcript, 70)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        35,
        evm_mulmod(
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 35)?, word(transcript, 49)?, q_mod()),
                        evm_mulmod(word(buf, 40)?, word(transcript, 61)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 42)?, word(transcript, 64)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 47)?, word(transcript, 71)?, q_mod()),
                q_mod(),
            ),
            word(buf, 9)?,
            q_mod(),
        ),
    )?;
    Ok(())
}

pub(super) fn step3(
    transcript: &[Word],
    _aux: &[Word],
    buf: &mut [Word],
) -> Result<(), ProofError> {
    set_word(
        buf,
        34,
        evm_addmod(
            word(buf, 35)?,
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 34)?, word(transcript, 49)?, q_mod()),
                        evm_mulmod(word(buf, 36)?, word(transcript, 61)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 39)?, word(transcript, 64)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 44)?, word(transcript, 71)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        23,
        evm_addmod(
            evm_mulmod(word(buf, 34)?, word(buf, 9)?, q_mod()),
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 23)?, word(transcript, 49)?, q_mod()),
                        evm_mulmod(word(buf, 26)?, word(transcript, 61)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 32)?, word(transcript, 64)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 37)?, word(transcript, 71)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(
        buf,
        19,
        evm_addmod(
            evm_mulmod(word(buf, 23)?, word(buf, 9)?, q_mod()),
            evm_addmod(
                evm_addmod(
                    evm_addmod(
                        evm_mulmod(word(buf, 20)?, word(transcript, 49)?, q_mod()),
                        evm_mulmod(word(buf, 30)?, word(transcript, 61)?, q_mod()),
                        q_mod(),
                    ),
                    evm_mulmod(word(buf, 27)?, word(transcript, 64)?, q_mod()),
                    q_mod(),
                ),
                evm_mulmod(word(buf, 19)?, word(transcript, 71)?, q_mod()),
                q_mod(),
            ),
            q_mod(),
        ),
    )?;
    set_word(buf, 20, evm_mulmod(word(buf, 17)?, word(buf, 29)?, q_mod()))?;
    set_word(
        buf,
        19,
        evm_mulmod(
            evm_addmod(
                evm_mulmod(
                    word(buf, 7)?,
                    evm_addmod(
                        evm_mulmod(word(buf, 7)?, word(buf, 24)?, q_mod()),
                        word(buf, 33)?,
                        q_mod(),
                    ),
                    q_mod(),
                ),
                word(buf, 19)?,
                q_mod(),
            ),
            word(buf, 20)?,
            q_mod(),
        ),
    )?;
    set_word(buf, 18, evm_addmod(word(buf, 18)?, word(buf, 19)?, q_mod()))?;
    let value_12_13_a = word_dec("1")?;
    let value_12_13_b =
        word_dec("21888242871839275222246405745257275088696311157297823662689037894645226208581")?;
    set_word(buf, 12, value_12_13_a)?;
    set_word(buf, 13, value_12_13_b)?;
    set_word(buf, 14, word(buf, 18)?)?;
    ecc_mul(buf, 12)?;
    set_word(buf, 19, evm_mulmod(word(buf, 7)?, word(buf, 7)?, q_mod()))?;
    let value_14_15_a = word(transcript, 0)?;
    let value_14_15_b = word(transcript, 1)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 20)?, word(buf, 19)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 2)?;
    let value_14_15_b = word(transcript, 3)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 20)?, word(buf, 7)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 4)?;
    let value_14_15_b = word(transcript, 5)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, word(buf, 20)?)?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 20, evm_mulmod(word(buf, 8)?, word(buf, 28)?, q_mod()))?;
    let value_14_15_a = word(transcript, 6)?;
    let value_14_15_b = word(transcript, 7)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 20)?, word(buf, 7)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 8)?;
    let value_14_15_b = word(transcript, 9)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, word(buf, 20)?)?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 20, evm_mulmod(word(buf, 8)?, word(buf, 8)?, q_mod()))?;
    set_word(buf, 23, evm_mulmod(word(buf, 20)?, word(buf, 25)?, q_mod()))?;
    set_word(buf, 24, evm_mulmod(word(buf, 19)?, word(buf, 19)?, q_mod()))?;
    set_word(buf, 25, evm_mulmod(word(buf, 24)?, word(buf, 7)?, q_mod()))?;
    let value_14_15_a = word(transcript, 10)?;
    let value_14_15_b = word(transcript, 11)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 23)?, word(buf, 25)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 12)?;
    let value_14_15_b = word(transcript, 13)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 23)?, word(buf, 24)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 26, evm_mulmod(word(buf, 19)?, word(buf, 7)?, q_mod()))?;
    let value_14_15_a = word(transcript, 14)?;
    let value_14_15_b = word(transcript, 15)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 23)?, word(buf, 26)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    set_word(
        buf,
        27,
        evm_mulmod(
            evm_mulmod(word(buf, 20)?, word(buf, 8)?, q_mod()),
            word(buf, 31)?,
            q_mod(),
        ),
    )?;
    set_word(buf, 28, evm_mulmod(word(buf, 24)?, word(buf, 24)?, q_mod()))?;
    set_word(buf, 29, evm_mulmod(word(buf, 28)?, word(buf, 28)?, q_mod()))?;
    set_word(buf, 30, evm_mulmod(word(buf, 29)?, word(buf, 28)?, q_mod()))?;
    set_word(buf, 31, evm_mulmod(word(buf, 30)?, word(buf, 24)?, q_mod()))?;
    set_word(buf, 32, evm_mulmod(word(buf, 31)?, word(buf, 19)?, q_mod()))?;
    let value_14_15_a = word(transcript, 16)?;
    let value_14_15_b = word(transcript, 17)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 32)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("4197764779765500619086905679346349595502678646650401496284249899640081263054")?;
    let value_14_15_b =
        word_dec("18735396249204036122862126795171053274142628440718554871209977871188710381062")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 30)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("20263923995110091594801969246165930232724240793772978764231804343146076859752")?;
    let value_14_15_b =
        word_dec("421652627040302342429968092955146532772337732135257753012244505824085349488")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 30)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("20316678354551864418735195238200484032571357391673966501704451507022329986473")?;
    let value_14_15_b =
        word_dec("12102513160157897850514330171721453385402608165962547679060939399639079969958")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 29)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("12131805172690818921224463503619358613621337457707771682409167766796037180283")?;
    let value_14_15_b =
        word_dec("13208913633412377005811323489943316350438867644531811596310619546646908788982")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 29)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 33, evm_mulmod(word(buf, 29)?, word(buf, 19)?, q_mod()))?;
    let value_14_15_a =
        word_dec("16384750106869563780750653698065597928235826011333158029841535278257525622364")?;
    let value_14_15_b =
        word_dec("13863360466940931629812474946052676345841354556483994318633110016394763233331")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 33)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 34, evm_mulmod(word(buf, 28)?, word(buf, 24)?, q_mod()))?;
    set_word(buf, 35, evm_mulmod(word(buf, 34)?, word(buf, 19)?, q_mod()))?;
    let value_14_15_a =
        word_dec("6524097008599549365123830772150400971059064924093724822109556526305030944372")?;
    let value_14_15_b =
        word_dec("11975134030697462184533554896933769702953392735855718845296166248150193106755")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 35)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("9690307389924373697716902955823195966385072356380965110177571012116884048920")?;
    let value_14_15_b =
        word_dec("19618456317950007497155487137872897961718693142124992521591736279635561929953")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 34)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("13750082717186074527102402301164120215816517966876017904216745309676991818024")?;
    let value_14_15_b =
        word_dec("20168282241077459436545457957891042994257112951499120117927799258527671693670")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 34)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 29, evm_mulmod(word(buf, 29)?, word(buf, 24)?, q_mod()))?;
    set_word(buf, 34, evm_mulmod(word(buf, 29)?, word(buf, 19)?, q_mod()))?;
    let value_14_15_a =
        word_dec("1481491789412622917814053383035511210413784440903980072586462802645485405009")?;
    let value_14_15_b =
        word_dec("2190066763502576188001544595293527324760841443296051421799231155294372343162")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 34)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("1576504727487329391333045229968578967695057954958719721922860567930063861790")?;
    let value_14_15_b =
        word_dec("10356720834062446187458923003371025310178336363085896428919562027823018977946")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 34)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("8172690645807961515243483619510424773280773072143424731910615647372686727269")?;
    let value_14_15_b =
        word_dec("6946490189576295074292967580315696138691199442472005465157927513655793111845")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 29)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("3313692799062629272001890825506968964421021335301702147428043313291462637368")?;
    let value_14_15_b =
        word_dec("3599028245088017049061971064414542879746315866711715822560899522222605065046")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 29)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("7182236695780970206389346028844263412451473246832442651054190543660105715259")?;
    let value_14_15_b =
        word_dec("12354471184501813978328098045884549982581713624717658557330518233536982619380")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 33)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 29, evm_mulmod(word(buf, 30)?, word(buf, 19)?, q_mod()))?;
    let value_14_15_a = word_dec("0")?;
    let value_14_15_b = word_dec("0")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 29)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("1995050330450656045594488181727773989777810898731449656158144562918905052849")?;
    let value_14_15_b =
        word_dec("19548346179324515640145098906356283639256238010242736427346569268582024448891")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 29)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    Ok(())
}

pub(super) fn step4(
    transcript: &[Word],
    _aux: &[Word],
    buf: &mut [Word],
) -> Result<Vec<Word>, ProofError> {
    let value_14_15_a =
        word_dec("3485352785218733130382545172103328837021794818319892061587750343874992026690")?;
    let value_14_15_b =
        word_dec("7631097809654549230473542512961848881196199304598008750331689483497279251726")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 35)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 122)?;
    let value_14_15_b = word(transcript, 123)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        q_mod_minus(evm_mulmod(
            evm_mulmod(
                word(buf, 17)?,
                evm_addmod(word(buf, 9)?, q_mod_minus(word(buf, 6)?)?, q_mod()),
                q_mod(),
            ),
            word(buf, 21)?,
            q_mod(),
        ))?,
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 124)?;
    let value_14_15_b = word(transcript, 125)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, word(buf, 9)?)?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 17, evm_mulmod(word(buf, 27)?, word(buf, 7)?, q_mod()))?;
    let value_14_15_a = word(transcript, 44)?;
    let value_14_15_b = word(transcript, 45)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 17)?,
            evm_mulmod(word(buf, 22)?, word(buf, 22)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 42)?;
    let value_14_15_b = word(transcript, 43)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 17)?, word(buf, 22)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 40)?;
    let value_14_15_b = word(transcript, 41)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, word(buf, 17)?)?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(buf, 0)?;
    let value_14_15_b = word(buf, 1)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 32)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 18)?;
    let value_14_15_b = word(transcript, 19)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 31)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 17, evm_mulmod(word(buf, 20)?, word(buf, 20)?, q_mod()))?;
    let value_14_15_a = word(transcript, 32)?;
    let value_14_15_b = word(transcript, 33)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, word(buf, 17)?)?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 34)?;
    let value_14_15_b = word(transcript, 35)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 23)?, word(buf, 7)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 20)?;
    let value_14_15_b = word(transcript, 21)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 31)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 36)?;
    let value_14_15_b = word(transcript, 37)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, word(buf, 23)?)?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 20, evm_mulmod(word(buf, 28)?, word(buf, 19)?, q_mod()))?;
    let value_14_15_a =
        word_dec("16013101360777447430391567575998918811509597004290763871753250359697657950684")?;
    let value_14_15_b =
        word_dec("18284031839732107412046231472706741488467955619222543589254217767130083585094")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 20)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("19737112949388897458588954660923088458903640709501932895808401103192630637886")?;
    let value_14_15_b =
        word_dec("10675054364325450202178813304652591601320231805686501913156439839611155541187")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 20)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("11075133387362804604013739655507111764020652526474683365602194077653197238272")?;
    let value_14_15_b =
        word_dec("1318109441292842640674212016592417370670910230830168691558759097428556833331")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 28)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("6493171341935202456206124325209040072103532752509851882039398057802652209155")?;
    let value_14_15_b =
        word_dec("5759392857488024763949679302755042832694498596039414370795425275927948962300")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 28)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 20, evm_mulmod(word(buf, 24)?, word(buf, 19)?, q_mod()))?;
    let value_14_15_a =
        word_dec("3434307301702672135490012972724498468724430053586845303435546131578132169565")?;
    let value_14_15_b =
        word_dec("9401556957552335892402756214239533149040282436268967679656060418659943805456")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(
        buf,
        16,
        evm_mulmod(
            word(buf, 27)?,
            evm_mulmod(word(buf, 20)?, word(buf, 7)?, q_mod()),
            q_mod(),
        ),
    )?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("20663073642495903566652507188978303698122574665635693523115271380298096913944")?;
    let value_14_15_b =
        word_dec("20626435045404010153820563667748941335090321171987700717901703112061387457625")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 20)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("2909633653439036467368225770107015305052480086354402433210031100584322224938")?;
    let value_14_15_b =
        word_dec("9113379992454044692611595843225590850770695728397706187195170007447718185937")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 25)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("12370723155217111550751604409229284398171581598248158174691848143324925475713")?;
    let value_14_15_b =
        word_dec("8485843117039022328500854142848443817737786116149217563496647527406108346263")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 24)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("14407351040403581664332334748264473379864203161240022353233241082150176386285")?;
    let value_14_15_b =
        word_dec("9844309975623883240590259440306357109543057870677302555594358299006757399285")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 26)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a =
        word_dec("18211387557073157961937239785723760864489490955530153678009805065113423060607")?;
    let value_14_15_b =
        word_dec("12247546671223471605851044424338710756724515356960756532877336632123969096339")?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 27)?, word(buf, 19)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 22)?;
    let value_14_15_b = word(transcript, 23)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 17)?, word(buf, 24)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    set_word(buf, 20, evm_mulmod(word(buf, 17)?, word(buf, 19)?, q_mod()))?;
    let value_14_15_a = word(transcript, 24)?;
    let value_14_15_b = word(transcript, 25)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 20)?, word(buf, 7)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 26)?;
    let value_14_15_b = word(transcript, 27)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, word(buf, 20)?)?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 28)?;
    let value_14_15_b = word(transcript, 29)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 17)?, word(buf, 7)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 30)?;
    let value_14_15_b = word(transcript, 31)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, evm_mulmod(word(buf, 23)?, word(buf, 19)?, q_mod()))?;
    ecc_mul_add(buf, 12)?;
    let value_14_15_a = word(transcript, 38)?;
    let value_14_15_b = word(transcript, 39)?;
    set_word(buf, 14, value_14_15_a)?;
    set_word(buf, 15, value_14_15_b)?;
    set_word(buf, 16, word(buf, 27)?)?;
    ecc_mul_add(buf, 12)?;
    Ok(vec![
        word(buf, 10)?,
        word(buf, 11)?,
        word(buf, 12)?,
        word(buf, 13)?,
    ])
}
