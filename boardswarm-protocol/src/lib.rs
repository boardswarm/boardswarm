use serde::de::value::{MapDeserializer, SeqDeserializer};

tonic::include_proto!("boardswarm");

/// Default port for boardswarm servers
pub const DEFAULT_PORT: u16 = 6653;

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Parameters(prost_types::Struct);

impl Parameters {
    pub fn insert(&mut self, k: String, v: ParamValue) {
        self.0.fields.insert(k, v.0);
    }
}

impl prost::Message for Parameters {
    fn encode_raw<B>(&self, buf: &mut B)
    where
        B: bytes::BufMut,
        Self: Sized,
    {
        self.0.encode_raw(buf)
    }

    fn merge_field<B>(
        &mut self,
        tag: u32,
        wire_type: prost::encoding::WireType,
        buf: &mut B,
        ctx: prost::encoding::DecodeContext,
    ) -> Result<(), prost::DecodeError>
    where
        B: bytes::Buf,
        Self: Sized,
    {
        self.0.merge_field(tag, wire_type, buf, ctx)
    }

    fn encoded_len(&self) -> usize {
        self.0.encoded_len()
    }

    fn clear(&mut self) {
        self.0.clear()
    }
}

macro_rules! value_visit_num {
    ($ty:ident : $visit:ident) => {
        fn $visit<E>(self, v: $ty) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(ParamValue::from(v as f64))
        }
    };
}

pub struct ParamValue(prost_types::Value);
impl From<&str> for ParamValue {
    fn from(s: &str) -> ParamValue {
        ParamValue::from(s.to_string())
    }
}

impl From<String> for ParamValue {
    fn from(s: String) -> ParamValue {
        ParamValue(prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(s)),
        })
    }
}

impl From<f64> for ParamValue {
    fn from(v: f64) -> ParamValue {
        ParamValue(prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(v as f64)),
        })
    }
}

impl From<bool> for ParamValue {
    fn from(b: bool) -> ParamValue {
        ParamValue(prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(b)),
        })
    }
}

impl From<prost_types::Struct> for ParamValue {
    fn from(s: prost_types::Struct) -> ParamValue {
        ParamValue(prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(s)),
        })
    }
}

impl From<Parameters> for ParamValue {
    fn from(p: Parameters) -> ParamValue {
        ParamValue::from(p.0)
    }
}

impl From<Vec<prost_types::Value>> for ParamValue {
    fn from(values: Vec<prost_types::Value>) -> ParamValue {
        ParamValue(prost_types::Value {
            kind: Some(prost_types::value::Kind::ListValue(
                prost_types::ListValue { values },
            )),
        })
    }
}

impl From<Vec<ParamValue>> for ParamValue {
    fn from(mut values: Vec<ParamValue>) -> ParamValue {
        let values: Vec<_> = values.drain(..).map(|v| v.0).collect();
        ParamValue::from(values)
    }
}

impl<'de> serde::Deserialize<'de> for ParamValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ValueVisitor {}
        impl<'de> serde::de::Visitor<'de> for ValueVisitor {
            type Value = ParamValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a value")
            }

            value_visit_num!(u8: visit_u8);
            value_visit_num!(u16: visit_u16);
            value_visit_num!(u32: visit_u32);
            value_visit_num!(u64: visit_u64);

            value_visit_num!(i8: visit_i8);
            value_visit_num!(i16: visit_i16);
            value_visit_num!(i32: visit_i32);
            value_visit_num!(i64: visit_i64);

            value_visit_num!(f32: visit_f32);
            value_visit_num!(f64: visit_f64);

            fn visit_bool<E>(self, b: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ParamValue::from(b))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ParamValue::from(v))
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(ParamValue::from(v))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut parameter = Parameters::default();
                while let Some((key, value)) = map.next_entry()? {
                    parameter.insert(key, value);
                }

                Ok(ParamValue::from(parameter))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let len = seq.size_hint().unwrap_or_default().min(4096);
                let mut values = Vec::with_capacity(len);
                while let Some(value) = seq.next_element::<ParamValue>()? {
                    values.push(value.0);
                }

                Ok(ParamValue::from(values))
            }
        }

        let visitor = ValueVisitor {};
        deserializer.deserialize_any(visitor)
    }
}

impl<'de> serde::de::IntoDeserializer<'de, serde::de::value::Error> for ParamValue {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self::Deserializer {
        self
    }
}

impl<'de> serde::Deserialize<'de> for Parameters {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StructVisitor {}
        impl<'de> serde::de::Visitor<'de> for StructVisitor {
            type Value = Parameters;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut parameters = Parameters::default();
                while let Some((key, value)) = map.next_entry()? {
                    parameters.insert(key, value);
                }

                Ok(parameters)
            }
        }

        let visitor = StructVisitor {};
        deserializer.deserialize_map(visitor)
    }
}

impl<'de> serde::Deserializer<'de> for Parameters {
    type Error = serde::de::value::Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let deserializer =
            MapDeserializer::new(self.0.fields.into_iter().map(|(k, v)| (k, ParamValue(v))));
        let map = visitor.visit_map(deserializer)?;
        //deserializer.end()?;
        Ok(map)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

macro_rules! deserialize_number_as_int {
    ($deserialize:ident:$visit:ident:$ty:ident) => {
        fn $deserialize<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: serde::de::Visitor<'de>,
        {
            if let Some(prost_types::value::Kind::NumberValue(v)) = self.0.kind {
                if v.fract() == 0.0 {
                    let v = v as i64;
                    if let Ok(v) = v.try_into() {
                        return visitor.$visit(v);
                    }
                }
            }
            self.deserialize_any(visitor)
        }
    };
}

impl<'de> serde::Deserializer<'de> for ParamValue {
    type Error = serde::de::value::Error;
    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        if let Some(v) = self.0.kind {
            match v {
                prost_types::value::Kind::NullValue(_) => visitor.visit_none(),
                prost_types::value::Kind::NumberValue(v) => visitor.visit_f64(v),
                prost_types::value::Kind::StringValue(s) => visitor.visit_string(s),
                prost_types::value::Kind::BoolValue(b) => visitor.visit_bool(b),
                prost_types::value::Kind::StructValue(p) => Parameters(p).deserialize_any(visitor),
                prost_types::value::Kind::ListValue(list) => {
                    let seq = SeqDeserializer::new(list.values.into_iter().map(ParamValue));
                    visitor.visit_seq(seq)
                }
            }
        } else {
            visitor.visit_none()
        }
    }

    /*
    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        if let Some(prost_types::value::Kind::NumberValue(v)) = self.0.kind {
            visitor.visit_u32(v as u32)
        } else {
            self.deserialize_any(visitor)
        }
    }
    */
    deserialize_number_as_int!(deserialize_i8: visit_i8: i8);
    deserialize_number_as_int!(deserialize_i16: visit_i16: i16);
    deserialize_number_as_int!(deserialize_i32: visit_i32: i32);
    deserialize_number_as_int!(deserialize_i64: visit_i64: i64);
    deserialize_number_as_int!(deserialize_u8: visit_u8: u8);
    deserialize_number_as_int!(deserialize_u16: visit_u16: u16);
    deserialize_number_as_int!(deserialize_u32: visit_u32: u32);
    deserialize_number_as_int!(deserialize_u64: visit_u64: u64);

    serde::forward_to_deserialize_any! {
        bool i128 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

#[cfg(test)]
mod test {
    use serde::Deserialize;

    use super::*;

    #[test]
    fn test_deserialize() {
        let p: Parameters = serde_json::from_str(
            r#"
        {
            "number": 115200,
            "bool": true,
            "string": "gnirts",
            "list": [ "a", 2 ],
            "struct": {
              "number": 115200,
              "bool": true,
              "string": "gnirts",
              "list": [ "a", 2 ]
            }
        }
        "#,
        )
        .unwrap();
        let mut expected = Parameters::default();
        expected.insert("number".to_string(), ParamValue::from(115200 as f64));
        expected.insert("bool".to_string(), ParamValue::from(true));
        expected.insert("string".to_string(), ParamValue::from("gnirts"));

        let list = vec![ParamValue::from("a"), ParamValue::from(2f64)];
        expected.insert("list".to_string(), ParamValue::from(list));

        let subparameters = expected.clone();
        expected.insert("struct".to_string(), ParamValue::from(subparameters));

        assert_eq!(p, expected);
    }

    #[test]
    fn test_deserializer() {
        let mut data = Parameters::default();
        data.insert("number".to_string(), ParamValue::from(115200 as f64));
        data.insert("bool".to_string(), ParamValue::from(true));
        data.insert("string".to_string(), ParamValue::from("gnirts"));

        let list = vec![ParamValue::from("a"), ParamValue::from("b")];
        data.insert("list".to_string(), ParamValue::from(list));

        let subparameters = data.clone();
        data.insert("struct".to_string(), ParamValue::from(subparameters));

        #[derive(Debug, PartialEq, Deserialize)]
        struct Test {
            number: f64,
            string: String,
            list: Vec<String>,
            #[serde(rename = "struct")]
            struct_: Test2,
        }
        #[derive(Debug, PartialEq, Deserialize)]
        struct Test2 {
            number: u32,
            string: String,
            list: Vec<String>,
        }

        let t = Test::deserialize(data).unwrap();
        assert_eq!(
            t,
            Test {
                number: 115200f64,
                string: "gnirts".to_string(),
                list: vec!["a".to_string(), "b".to_string()],
                struct_: Test2 {
                    number: 115200,
                    string: "gnirts".to_string(),
                    list: vec!["a".to_string(), "b".to_string()]
                }
            }
        );
    }
}
