/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::dom::bindings::codegen::Bindings::CSSStyleDeclarationBinding::CSSStyleDeclarationMethods;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::ElementBinding::ElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLElementBinding::HTMLElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLInputElementBinding::HTMLInputElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLOptionElementBinding::HTMLOptionElementMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::codegen::Bindings::XMLSerializerBinding::XMLSerializerMethods;
use crate::dom::bindings::conversions::{
    ConversionResult, FromJSValConvertible, StringificationBehavior,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::element::Element;
use crate::dom::globalscope::GlobalScope;
use crate::dom::htmlelement::HTMLElement;
use crate::dom::htmliframeelement::HTMLIFrameElement;
use crate::dom::htmlinputelement::HTMLInputElement;
use crate::dom::htmloptionelement::HTMLOptionElement;
use crate::dom::node::{window_from_node, Node, ShadowIncluding};
use crate::dom::window::Window;
use crate::dom::xmlserializer::XMLSerializer;
use crate::script_thread::Documents;
use cookie::Cookie;
use euclid::{Point2D, Rect, Size2D};
use hyper_serde::Serde;
use ipc_channel::ipc::{self, IpcSender};
use js::jsapi::JSContext;
use js::jsval::UndefinedValue;
use js::rust::HandleValue;
use msg::constellation_msg::BrowsingContextId;
use msg::constellation_msg::PipelineId;
use net_traits::CookieSource::{NonHTTP, HTTP};
use net_traits::CoreResourceMsg::{DeleteCookies, GetCookiesDataForUrl, SetCookieForUrl};
use net_traits::IpcSend;
use script_traits::webdriver_msg::WebDriverCookieError;
use script_traits::webdriver_msg::{
    WebDriverFrameId, WebDriverJSError, WebDriverJSResult, WebDriverJSValue,
};
use servo_url::ServoUrl;

fn find_node_by_unique_id(
    documents: &Documents,
    pipeline: PipelineId,
    node_id: String,
) -> Option<DomRoot<Node>> {
    documents.find_document(pipeline).and_then(|document| {
        document
            .upcast::<Node>()
            .traverse_preorder(ShadowIncluding::Yes)
            .find(|candidate| candidate.unique_id() == node_id)
    })
}

#[allow(unsafe_code)]
pub unsafe fn jsval_to_webdriver(cx: *mut JSContext, val: HandleValue) -> WebDriverJSResult {
    if val.get().is_undefined() {
        Ok(WebDriverJSValue::Undefined)
    } else if val.get().is_boolean() {
        Ok(WebDriverJSValue::Boolean(val.get().to_boolean()))
    } else if val.get().is_double() || val.get().is_int32() {
        Ok(WebDriverJSValue::Number(
            match FromJSValConvertible::from_jsval(cx, val, ()).unwrap() {
                ConversionResult::Success(c) => c,
                _ => unreachable!(),
            },
        ))
    } else if val.get().is_string() {
        //FIXME: use jsstring_to_str when jsval grows to_jsstring
        let string: DOMString =
            match FromJSValConvertible::from_jsval(cx, val, StringificationBehavior::Default)
                .unwrap()
            {
                ConversionResult::Success(c) => c,
                _ => unreachable!(),
            };
        Ok(WebDriverJSValue::String(String::from(string)))
    } else if val.get().is_null() {
        Ok(WebDriverJSValue::Null)
    } else {
        Err(WebDriverJSError::UnknownType)
    }
}

#[allow(unsafe_code)]
pub fn handle_execute_script(
    window: Option<DomRoot<Window>>,
    eval: String,
    reply: IpcSender<WebDriverJSResult>,
) {
    match window {
        Some(window) => {
            let result = unsafe {
                let cx = window.get_cx();
                rooted!(in(cx) let mut rval = UndefinedValue());
                window
                    .upcast::<GlobalScope>()
                    .evaluate_js_on_global_with_result(&eval, rval.handle_mut());
                jsval_to_webdriver(cx, rval.handle())
            };

            reply.send(result).unwrap();
        },
        None => {
            reply
                .send(Err(WebDriverJSError::BrowsingContextNotFound))
                .unwrap();
        },
    }
}

pub fn handle_execute_async_script(
    window: Option<DomRoot<Window>>,
    eval: String,
    reply: IpcSender<WebDriverJSResult>,
) {
    match window {
        Some(window) => {
            let cx = window.get_cx();
            window.set_webdriver_script_chan(Some(reply));
            rooted!(in(cx) let mut rval = UndefinedValue());
            window
                .upcast::<GlobalScope>()
                .evaluate_js_on_global_with_result(&eval, rval.handle_mut());
        },
        None => {
            reply
                .send(Err(WebDriverJSError::BrowsingContextNotFound))
                .unwrap();
        },
    }
}

pub fn handle_get_browsing_context_id(
    documents: &Documents,
    pipeline: PipelineId,
    webdriver_frame_id: WebDriverFrameId,
    reply: IpcSender<Result<BrowsingContextId, ()>>,
) {
    let result = match webdriver_frame_id {
        WebDriverFrameId::Short(_) => {
            // This isn't supported yet
            Err(())
        },
        WebDriverFrameId::Element(x) => find_node_by_unique_id(documents, pipeline, x)
            .and_then(|node| {
                node.downcast::<HTMLIFrameElement>()
                    .and_then(|elem| elem.browsing_context_id())
            })
            .ok_or(()),
        WebDriverFrameId::Parent => documents
            .find_window(pipeline)
            .and_then(|window| {
                window
                    .window_proxy()
                    .parent()
                    .map(|parent| parent.browsing_context_id())
            })
            .ok_or(()),
    };

    reply.send(result).unwrap()
}

pub fn handle_find_element_css(
    documents: &Documents,
    pipeline: PipelineId,
    selector: String,
    reply: IpcSender<Result<Option<String>, ()>>,
) {
    let node_id = documents
        .find_document(pipeline)
        .ok_or(())
        .and_then(|doc| doc.QuerySelector(DOMString::from(selector)).map_err(|_| ()))
        .map(|node| node.map(|x| x.upcast::<Node>().unique_id()));
    reply.send(node_id).unwrap();
}

pub fn handle_find_element_tag_name(
    documents: &Documents,
    pipeline: PipelineId,
    selector: String,
    reply: IpcSender<Result<Option<String>, ()>>,
) {
    let node_id = documents
        .find_document(pipeline)
        .ok_or(())
        .and_then(|doc| {
            Ok(doc
                .GetElementsByTagName(DOMString::from(selector))
                .elements_iter()
                .next())
        })
        .map(|node| node.map(|x| x.upcast::<Node>().unique_id()));
    reply.send(node_id).unwrap();
}

pub fn handle_find_elements_css(
    documents: &Documents,
    pipeline: PipelineId,
    selector: String,
    reply: IpcSender<Result<Vec<String>, ()>>,
) {
    let node_ids = documents
        .find_document(pipeline)
        .ok_or(())
        .and_then(|doc| {
            doc.QuerySelectorAll(DOMString::from(selector))
                .map_err(|_| ())
        })
        .map(|nodes| {
            nodes
                .iter()
                .map(|x| x.upcast::<Node>().unique_id())
                .collect()
        });
    reply.send(node_ids).unwrap();
}

pub fn handle_find_elements_tag_name(
    documents: &Documents,
    pipeline: PipelineId,
    selector: String,
    reply: IpcSender<Result<Vec<String>, ()>>,
) {
    let node_ids = documents
        .find_document(pipeline)
        .ok_or(())
        .and_then(|doc| Ok(doc.GetElementsByTagName(DOMString::from(selector))))
        .map(|nodes| {
            nodes
                .elements_iter()
                .map(|x| x.upcast::<Node>().unique_id())
                .collect::<Vec<String>>()
        });
    reply.send(node_ids).unwrap();
}

pub fn handle_find_element_element_css(
    documents: &Documents,
    pipeline: PipelineId,
    element_id: String,
    selector: String,
    reply: IpcSender<Result<Option<String>, ()>>,
) {
    let node_id = find_node_by_unique_id(documents, pipeline, element_id)
        .ok_or(())
        .and_then(|node| {
            node.query_selector(DOMString::from(selector))
                .map_err(|_| ())
        })
        .map(|node| node.map(|x| x.upcast::<Node>().unique_id()));
    reply.send(node_id).unwrap();
}

pub fn handle_find_element_element_tag_name(
    documents: &Documents,
    pipeline: PipelineId,
    element_id: String,
    selector: String,
    reply: IpcSender<Result<Option<String>, ()>>,
) {
    let node_id = find_node_by_unique_id(documents, pipeline, element_id)
        .ok_or(())
        .and_then(|node| match node.downcast::<Element>() {
            Some(elem) => Ok(elem
                .GetElementsByTagName(DOMString::from(selector))
                .elements_iter()
                .next()),
            None => Err(()),
        })
        .map(|node| node.map(|x| x.upcast::<Node>().unique_id()));
    reply.send(node_id).unwrap();
}

pub fn handle_find_element_elements_css(
    documents: &Documents,
    pipeline: PipelineId,
    element_id: String,
    selector: String,
    reply: IpcSender<Result<Vec<String>, ()>>,
) {
    let node_ids = find_node_by_unique_id(documents, pipeline, element_id)
        .ok_or(())
        .and_then(|node| {
            node.query_selector_all(DOMString::from(selector))
                .map_err(|_| ())
        })
        .map(|nodes| {
            nodes
                .iter()
                .map(|x| x.upcast::<Node>().unique_id())
                .collect()
        });
    reply.send(node_ids).unwrap();
}

pub fn handle_find_element_elements_tag_name(
    documents: &Documents,
    pipeline: PipelineId,
    element_id: String,
    selector: String,
    reply: IpcSender<Result<Vec<String>, ()>>,
) {
    let node_ids = find_node_by_unique_id(documents, pipeline, element_id)
        .ok_or(())
        .and_then(|node| match node.downcast::<Element>() {
            Some(elem) => Ok(elem.GetElementsByTagName(DOMString::from(selector))),
            None => Err(()),
        })
        .map(|nodes| {
            nodes
                .elements_iter()
                .map(|x| x.upcast::<Node>().unique_id())
                .collect::<Vec<String>>()
        });
    reply.send(node_ids).unwrap();
}

pub fn handle_focus_element(
    documents: &Documents,
    pipeline: PipelineId,
    element_id: String,
    reply: IpcSender<Result<(), ()>>,
) {
    reply
        .send(
            match find_node_by_unique_id(documents, pipeline, element_id) {
                Some(ref node) => {
                    match node.downcast::<HTMLElement>() {
                        Some(ref elem) => {
                            // Need a way to find if this actually succeeded
                            elem.Focus();
                            Ok(())
                        },
                        None => Err(()),
                    }
                },
                None => Err(()),
            },
        )
        .unwrap();
}

pub fn handle_get_active_element(
    documents: &Documents,
    pipeline: PipelineId,
    reply: IpcSender<Option<String>>,
) {
    reply
        .send(
            documents
                .find_document(pipeline)
                .and_then(|doc| doc.GetActiveElement())
                .map(|elem| elem.upcast::<Node>().unique_id()),
        )
        .unwrap();
}

pub fn handle_get_page_source(
    documents: &Documents,
    pipeline: PipelineId,
    reply: IpcSender<Result<String, ()>>,
) {
    reply
        .send(documents.find_document(pipeline).ok_or(()).and_then(|doc| {
            match doc.GetDocumentElement() {
                Some(elem) => match elem.GetOuterHTML() {
                    Ok(source) => Ok(source.to_string()),
                    Err(_) => {
                        match XMLSerializer::new(doc.window())
                            .SerializeToString(elem.upcast::<Node>())
                        {
                            Ok(source) => Ok(source.to_string()),
                            Err(_) => Err(()),
                        }
                    },
                },
                None => Err(()),
            }
        }))
        .unwrap();
}

pub fn handle_get_cookies(
    documents: &Documents,
    pipeline: PipelineId,
    reply: IpcSender<Vec<Serde<Cookie<'static>>>>,
) {
    // TODO: Return an error if the pipeline doesn't exist?
    let cookies = match documents.find_document(pipeline) {
        None => Vec::new(),
        Some(document) => {
            let url = document.url();
            let (sender, receiver) = ipc::channel().unwrap();
            let _ = document
                .window()
                .upcast::<GlobalScope>()
                .resource_threads()
                .send(GetCookiesDataForUrl(url, sender, NonHTTP));
            receiver.recv().unwrap()
        },
    };
    reply.send(cookies).unwrap();
}

// https://w3c.github.io/webdriver/webdriver-spec.html#get-cookie
pub fn handle_get_cookie(
    documents: &Documents,
    pipeline: PipelineId,
    name: String,
    reply: IpcSender<Vec<Serde<Cookie<'static>>>>,
) {
    // TODO: Return an error if the pipeline doesn't exist?
    let cookies = match documents.find_document(pipeline) {
        None => Vec::new(),
        Some(document) => {
            let url = document.url();
            let (sender, receiver) = ipc::channel().unwrap();
            let _ = document
                .window()
                .upcast::<GlobalScope>()
                .resource_threads()
                .send(GetCookiesDataForUrl(url, sender, NonHTTP));
            receiver.recv().unwrap()
        },
    };
    reply
        .send(cookies.into_iter().filter(|c| c.name() == &*name).collect())
        .unwrap();
}

// https://w3c.github.io/webdriver/webdriver-spec.html#add-cookie
pub fn handle_add_cookie(
    documents: &Documents,
    pipeline: PipelineId,
    cookie: Cookie<'static>,
    reply: IpcSender<Result<(), WebDriverCookieError>>,
) {
    // TODO: Return a different error if the pipeline doesn't exist?
    let document = match documents.find_document(pipeline) {
        Some(document) => document,
        None => {
            return reply
                .send(Err(WebDriverCookieError::UnableToSetCookie))
                .unwrap();
        },
    };
    let url = document.url();
    let method = if cookie.http_only().unwrap_or(false) {
        HTTP
    } else {
        NonHTTP
    };

    let domain = cookie.domain().map(ToOwned::to_owned);
    reply
        .send(match (document.is_cookie_averse(), domain) {
            (true, _) => Err(WebDriverCookieError::InvalidDomain),
            (false, Some(ref domain)) if url.host_str().map(|x| x == domain).unwrap_or(false) => {
                let _ = document
                    .window()
                    .upcast::<GlobalScope>()
                    .resource_threads()
                    .send(SetCookieForUrl(url, Serde(cookie), method));
                Ok(())
            },
            (false, None) => {
                let _ = document
                    .window()
                    .upcast::<GlobalScope>()
                    .resource_threads()
                    .send(SetCookieForUrl(url, Serde(cookie), method));
                Ok(())
            },
            (_, _) => Err(WebDriverCookieError::UnableToSetCookie),
        })
        .unwrap();
}

pub fn handle_delete_cookies(
    documents: &Documents,
    pipeline: PipelineId,
    reply: IpcSender<Result<(), ()>>,
) {
    let document = match documents.find_document(pipeline) {
        Some(document) => document,
        None => {
            return reply.send(Err(())).unwrap();
        },
    };
    let url = document.url();
    document
        .window()
        .upcast::<GlobalScope>()
        .resource_threads()
        .send(DeleteCookies(url))
        .unwrap();
    let _ = reply.send(Ok(()));
}

pub fn handle_get_title(documents: &Documents, pipeline: PipelineId, reply: IpcSender<String>) {
    // TODO: Return an error if the pipeline doesn't exist.
    let title = documents
        .find_document(pipeline)
        .map(|doc| String::from(doc.Title()))
        .unwrap_or_default();
    reply.send(title).unwrap();
}

pub fn handle_get_rect(
    documents: &Documents,
    pipeline: PipelineId,
    element_id: String,
    reply: IpcSender<Result<Rect<f64>, ()>>,
) {
    reply
        .send(
            match find_node_by_unique_id(documents, pipeline, element_id) {
                Some(elem) => {
                    // https://w3c.github.io/webdriver/webdriver-spec.html#dfn-calculate-the-absolute-position
                    match elem.downcast::<HTMLElement>() {
                        Some(html_elem) => {
                            // Step 1
                            let mut x = 0;
                            let mut y = 0;

                            let mut offset_parent = html_elem.GetOffsetParent();

                            // Step 2
                            while let Some(element) = offset_parent {
                                offset_parent = match element.downcast::<HTMLElement>() {
                                    Some(elem) => {
                                        x += elem.OffsetLeft();
                                        y += elem.OffsetTop();
                                        elem.GetOffsetParent()
                                    },
                                    None => None,
                                };
                            }
                            // Step 3
                            Ok(Rect::new(
                                Point2D::new(x as f64, y as f64),
                                Size2D::new(
                                    html_elem.OffsetWidth() as f64,
                                    html_elem.OffsetHeight() as f64,
                                ),
                            ))
                        },
                        None => Err(()),
                    }
                },
                None => Err(()),
            },
        )
        .unwrap();
}

pub fn handle_get_text(
    documents: &Documents,
    pipeline: PipelineId,
    node_id: String,
    reply: IpcSender<Result<String, ()>>,
) {
    reply
        .send(match find_node_by_unique_id(documents, pipeline, node_id) {
            Some(ref node) => Ok(node.GetTextContent().map_or("".to_owned(), String::from)),
            None => Err(()),
        })
        .unwrap();
}

pub fn handle_get_name(
    documents: &Documents,
    pipeline: PipelineId,
    node_id: String,
    reply: IpcSender<Result<String, ()>>,
) {
    reply
        .send(match find_node_by_unique_id(documents, pipeline, node_id) {
            Some(node) => Ok(String::from(node.downcast::<Element>().unwrap().TagName())),
            None => Err(()),
        })
        .unwrap();
}

pub fn handle_get_attribute(
    documents: &Documents,
    pipeline: PipelineId,
    node_id: String,
    name: String,
    reply: IpcSender<Result<Option<String>, ()>>,
) {
    reply
        .send(match find_node_by_unique_id(documents, pipeline, node_id) {
            Some(node) => Ok(node
                .downcast::<Element>()
                .unwrap()
                .GetAttribute(DOMString::from(name))
                .map(String::from)),
            None => Err(()),
        })
        .unwrap();
}

pub fn handle_get_css(
    documents: &Documents,
    pipeline: PipelineId,
    node_id: String,
    name: String,
    reply: IpcSender<Result<String, ()>>,
) {
    reply
        .send(match find_node_by_unique_id(documents, pipeline, node_id) {
            Some(node) => {
                let window = window_from_node(&*node);
                let elem = node.downcast::<Element>().unwrap();
                Ok(String::from(
                    window
                        .GetComputedStyle(&elem, None)
                        .GetPropertyValue(DOMString::from(name)),
                ))
            },
            None => Err(()),
        })
        .unwrap();
}

pub fn handle_get_url(documents: &Documents, pipeline: PipelineId, reply: IpcSender<ServoUrl>) {
    // TODO: Return an error if the pipeline doesn't exist.
    let url = documents
        .find_document(pipeline)
        .map(|document| document.url())
        .unwrap_or_else(|| ServoUrl::parse("about:blank").expect("infallible"));
    reply.send(url).unwrap();
}

pub fn handle_is_enabled(
    documents: &Documents,
    pipeline: PipelineId,
    element_id: String,
    reply: IpcSender<Result<bool, ()>>,
) {
    reply
        .send(
            match find_node_by_unique_id(&documents, pipeline, element_id) {
                Some(ref node) => match node.downcast::<Element>() {
                    Some(elem) => Ok(elem.enabled_state()),
                    None => Err(()),
                },
                None => Err(()),
            },
        )
        .unwrap();
}

pub fn handle_is_selected(
    documents: &Documents,
    pipeline: PipelineId,
    element_id: String,
    reply: IpcSender<Result<bool, ()>>,
) {
    reply
        .send(
            match find_node_by_unique_id(documents, pipeline, element_id) {
                Some(ref node) => {
                    if let Some(input_element) = node.downcast::<HTMLInputElement>() {
                        Ok(input_element.Checked())
                    } else if let Some(option_element) = node.downcast::<HTMLOptionElement>() {
                        Ok(option_element.Selected())
                    } else if node.is::<HTMLElement>() {
                        Ok(false) // regular elements are not selectable
                    } else {
                        Err(())
                    }
                },
                None => Err(()),
            },
        )
        .unwrap();
}
