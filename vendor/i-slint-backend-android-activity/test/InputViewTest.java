// Slint's text input view as the phone's keyboard uses it, run on the phone
// (tools/test-android-input.sh). The keyboard had begun a batch of edits when
// the field's type changed (the passphrase's eye); Android gave it a new
// connection and dropped the old one's end of the batch. What it types after
// must still reach the window. And a keyboard composing in a password field
// (Android 10's own, in a BYD car) has the whole word given as typed.

import android.content.Context;
import android.os.Looper;
import android.text.InputType;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import java.lang.reflect.Field;

public class InputViewTest {
    // The text the view last gave the window (Slint's native call, absent
    // here, fails after it's set).
    static String given(SlintInputView view) throws Exception {
        Field f = SlintInputView.class.getDeclaredField("mText");
        f.setAccessible(true);
        return (String) f.get(view);
    }

    static void set(Class<?> c, Object o, String name, Object value) throws Exception {
        Field f = c.getDeclaredField(name);
        f.setAccessible(true);
        f.set(o, value);
    }

    // The view as its constructor makes it, but without View's, which wants
    // the window manager an app has and this process hasn't.
    static SlintInputView made(Context context) throws Exception {
        Class<?> unsafe = Class.forName("sun.misc.Unsafe");
        Field theUnsafe = unsafe.getDeclaredField("theUnsafe");
        theUnsafe.setAccessible(true);
        SlintInputView view = (SlintInputView) unsafe.getMethod("allocateInstance", Class.class)
                .invoke(theUnsafe.get(null), SlintInputView.class);
        set(android.view.View.class, view, "mContext", context);
        set(SlintInputView.class, view, "mText", "");
        set(SlintInputView.class, view, "mInputType", InputType.TYPE_CLASS_TEXT);
        set(SlintInputView.class, view, "selectionRect", new android.graphics.Rect());
        Class<?> editable = Class.forName("SlintInputView$SlintEditable");
        java.lang.reflect.Constructor<?> make = editable.getDeclaredConstructor(SlintInputView.class);
        make.setAccessible(true);
        set(SlintInputView.class, view, "mEditable", make.newInstance(view));
        return view;
    }

    static void type(InputConnection ic, String text) {
        try {
            ic.commitText(text, 1);
        } catch (UnsatisfiedLinkError e) {
            // Given to the window: Slint's own library isn't loaded here.
        }
    }

    static void compose(InputConnection ic, String text) {
        try {
            ic.setComposingText(text, 1);
        } catch (UnsatisfiedLinkError e) {
            // Given to the window, as above.
        }
    }

    static int field(SlintInputView view, String name) throws Exception {
        Field f = SlintInputView.class.getDeclaredField(name);
        f.setAccessible(true);
        return f.getInt(view);
    }

    public static void main(String[] args) throws Exception {
        Looper.prepareMainLooper();
        Class<?> thread = Class.forName("android.app.ActivityThread");
        Object system = thread.getMethod("systemMain").invoke(null);
        Context context = (Context) thread.getMethod("getSystemContext").invoke(system);

        SlintInputView view = made(context);
        int password = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD;
        int shown = InputType.TYPE_CLASS_TEXT;
        view.setText("", 0, 0, 0, 0, password);

        InputConnection first = view.onCreateInputConnection(new EditorInfo());
        type(first, "a");
        String before = given(view);

        // A batch begun, then the eye: the type changed, a new connection,
        // and the first's endBatchEdit never comes.
        first.beginBatchEdit();
        view.setText(before, 1, 1, 1, 1, shown);
        InputConnection second = view.onCreateInputConnection(new EditorInfo());
        type(second, "b");
        String after = given(view);

        // Were the old end to come late after all, typing still goes on.
        first.endBatchEdit();
        type(second, "c");
        String later = given(view);

        // A keyboard that composes even in a password field (Android 10's
        // own, in a car): the word it's composing is given to the window as
        // typed, not as a preedit Slint keeps out of the field's text.
        SlintInputView secret = made(context);
        secret.setText("", 0, 0, 0, 0, password);
        InputConnection keys = secret.onCreateInputConnection(new EditorInfo());
        compose(keys, "pass");
        boolean wholeInPassword = given(secret).equals("pass") && field(secret, "mPreeditStart") == -1
                && field(secret, "mPreeditEnd") == -1;
        // Elsewhere composing is shown as it was.
        SlintInputView plain = made(context);
        plain.setText("", 0, 0, 0, 0, shown);
        InputConnection words = plain.onCreateInputConnection(new EditorInfo());
        compose(words, "word");
        boolean composedInText = field(plain, "mPreeditStart") == 0 && field(plain, "mPreeditEnd") == 4;

        boolean ok = before.equals("a") && after.equals("ab") && later.equals("abc") && wholeInPassword
                && composedInText;
        System.out.println("typed a: given \"" + before + "\"; eye, typed b: given \"" + after
                + "\"; the old end late, typed c: given \"" + later + "\"; composing in a password field: "
                + (wholeInPassword ? "given as typed" : "a preedit") + "; in a text field: "
                + (composedInText ? "a preedit" : "not"));
        System.out.println(ok ? "PASSED" : "FAILED");
        System.exit(ok ? 0 : 1);
    }
}
